// The live weather store: the latest current-conditions reading of every
// field, arbitrated per field across every source on the bus, plus a
// watch channel the SSE endpoint subscribes to so browsers see updates
// the moment one lands. arc-swap gives a copy-on-write Arc<Snapshot> so
// handlers read the current state without taking a lock.
//
// One writer path. The Tempest UDP listener used to write this store
// directly through its own 280-line arbiter (`apply_obs`), racing the
// bus bridge for the same fields behind a write gate that existed only
// because there were two of them. Every source now publishes on the bus
// under its config id, the snapshot bridge is the one caller of
// `apply_source_fields`, `apply_strikes` and `apply_identity`, and the
// station-only readings (wind lull, the 3 s rapid wind, battery, the
// precipitation type) are ordinary bus fields.

use crate::tempest::packets::StrikeEvent;
use serde::{Deserialize, Serialize};

#[cfg(feature = "ssr")]
use {
    crate::weather::arbitration::{field_owner_key, owner_is_stale},
    crate::weather::derived::{feels_like_f, wet_bulb_c},
    arc_swap::ArcSwap,
    std::collections::{HashMap, VecDeque},
    std::sync::{Arc, Mutex},
    tokio::sync::watch,
};

/// One immutable snapshot of every value the dashboard renders. Rebuilt on
/// each Tempest packet and atomically swapped into the store. Cheap to
/// clone (it's `Arc`-wrapped before any client touches it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub last_packet_epoch: i64,
    pub air_temp_f: f64,
    pub feels_like_f: f64,
    pub dew_point_f: f64,
    pub wet_bulb_f: f64,
    pub rh_pct: f64,
    pub pressure_inhg: f64,
    pub pressure_trend_inhg: Vec<(i64, f64)>,
    pub wind_lull_mph: f64,
    pub wind_avg_mph: f64,
    pub wind_gust_mph: f64,
    pub wind_dir_deg: f64,
    pub rapid_wind_mph: f64,
    pub rapid_wind_dir: f64,
    pub illuminance_lx: f64,
    pub uv_index: f64,
    pub solar_w_m2: f64,
    pub rain_in_last_min: f64,
    pub rain_in_today: f64,
    pub rain_intensity_in_hr: f64,
    /// Reference evapotranspiration today (mm). From a source that reports ET0
    /// directly (HA-passthrough `et0today` / MQTT `et0_today` / Open-Meteo) or
    /// the native ET0 engine. 0.0 = unknown (engine falls back).
    pub et0_today: f64,
    /// Instantaneous flow (US gpm) from a flow meter on a controller or a
    /// standalone pulse meter, plus cumulative flow today (US gal).
    pub flow_gpm: f64,
    pub flow_total_gal_today: f64,
    /// Probability of precipitation (%) from a forecast source's current step.
    /// `None` until a configured source actually provides Pop: a bare 0 here
    /// read as "certainly no rain" on installs whose sources never report it
    /// (the lightning_avg_dist_mi shape). Serialized null so HA shows unknown.
    #[serde(default)]
    pub pop_pct: Option<f64>,
    /// Leaf wetness (%) from a leaf-wetness sensor (Davis WLL soil/leaf,
    /// Ecowitt WH35). Display + history only. `None` = unknown / not reported:
    /// a bare 0.0 published as a confident bone-dry canopy reading between
    /// integration setup and the sensor's first report (same defect shape as
    /// `lightning_avg_dist_mi` above, same resolution).
    #[serde(default)]
    pub leaf_wetness_pct: Option<f64>,
    pub precip_type: u8, // 0=none 1=rain 2=hail
    pub lightning_count_last_min: u32,
    /// Strikes in the rolling one-hour buffer. DERIVED from that buffer on
    /// every write path (see apply_obs / apply_strikes), never carried
    /// forward: a carried value froze the counter at its last storm total
    /// once strikes stopped, which left a `numeric_state above: 0` alert
    /// permanently armed and silent.
    pub lightning_strikes_last_hour: u32,
    pub lightning_recent: Vec<StrikeEvent>,
    /// Average distance of the strikes detected in the reporting interval,
    /// miles. `None` when the interval had no strikes: the stations report a
    /// bare 0 there, and publishing that on a distance channel reads as a
    /// strike directly overhead (the most alarming value possible) instead of
    /// "no reading". For a distance that persists between strikes, use
    /// `last_strike_distance_mi`.
    #[serde(default)]
    pub lightning_avg_dist_mi: Option<f64>,
    /// Distance to the most recent strike still inside the one-hour buffer,
    /// miles. Sticky across quiet intervals and decays to `None` when the
    /// last strike ages out.
    pub last_strike_distance_mi: Option<f64>,
    pub last_strike_epoch: Option<i64>,
    pub battery_v: f64,
    pub battery_pct: f64,
    pub station_serial: String,
    pub hub_serial: String,
    /// Display name of the source currently driving these current-conditions
    /// (the source's config id, or "Demo" in the demo feeder). Empty
    /// only before the first reading. Lets the UI show real provenance instead
    /// of assuming Tempest.
    pub source_label: String,
    /// Priority of the live source that currently OWNS these current
    /// conditions. The arbiter lets a strictly-higher-priority live source take
    /// over, and a same-or-lower one only when the owner goes stale. 0 until a
    /// live source claims it. (A3 multi-source current-conditions arbitration.)
    #[serde(default)]
    pub owner_priority: i32,
    /// Epoch of the last LIVE write of each engine-critical field (0 = never).
    /// Legacy LAN-only freshness metadata; selected current fields also carry PER
    /// FIELD instead of the whole-snapshot last_packet_epoch, so a field that is
    /// only forecast-filled (or never provided by a live source while a partial
    /// live source keeps the snapshot "fresh") is NOT treated as a live station
    /// reading in a run/skip decision.
    #[serde(default)]
    pub air_temp_live_epoch: i64,
    #[serde(default)]
    pub wind_live_epoch: i64,
    #[serde(default)]
    pub rh_live_epoch: i64,
    /// Epoch of the last LIVE write of the current-rain fields (rain intensity /
    /// rain type), set ONLY by live writers (live_current=true). The engine's
    /// "currently raining" path consults this PER FIELD instead of the whole-
    /// snapshot last_packet_epoch, so a stale Open-Meteo current-precip value
    /// (a forecast fill, live_current=false) can never read as live station
    /// rain and hard-skip a dry day, even while a partial barometer-only live
    /// source keeps last_packet_epoch fresh. A regression guard.
    #[serde(default)]
    pub rain_live_epoch: i64,
    /// TRUE when a real LIVE local weather station is currently present and
    /// producing: at least one current-conditions field is owned by a
    /// `live_current=true` source (Tempest UDP, Ecowitt, Davis, Netatmo, YoLink,
    /// an MQTT station, demo, ...). FALSE for a cloud-only install where only a
    /// forecast source (Open-Meteo current, `live_current=false`) fills the
    /// current conditions.
    ///
    /// This is the canonical "is there a station?" signal. The DISPLAY layer
    /// reads `!has_live_station` as the cloud-only test, REPLACING the old
    /// Tempest-only `station_serial.is_empty() && battery_v <= 0.0` heuristic,
    /// which misclassified an Ecowitt/Davis/MQTT live station (no Tempest serial,
    /// no battery voltage) as cloud-only. Set by any LIVE writer: `apply_obs`
    /// always, and `apply_source_fields` whenever a `live_current=true` source
    /// actually claims a field. A forecast-only fill never sets it, so an
    /// Open-Meteo-only deployment keeps it `false`. Once any live source has
    /// claimed a field it stays `true` (carried forward via `..prev`), so a
    /// momentary gap between station packets does not flap it back to cloud-only.
    #[serde(default)]
    pub has_live_station: bool,
    /// LOCAL calendar-day ordinal (num_days_from_ce) of the day
    /// `rain_in_today` currently accumulates for, stamped by every
    /// writer of that field (UDP day bucket, bus fill's own valid
    /// epoch). 0 = no writer yet. Server-internal bookkeeping: the
    /// observations-ledger writer gates its day-max upsert on this
    /// matching the row's date, so the first ticks after local midnight
    /// (before the accumulator resets) can never pin yesterday's total
    /// onto the new day's row. Never serialized.
    #[serde(skip)]
    pub rain_today_day_ordinal: i32,
    /// The source whose rain-today reading looks like per-minute rain
    /// rather than a since-midnight accumulation.
    ///
    /// Set when a live source reports a total that FELL inside the same
    /// local day. A real accumulator cannot do that; a per-minute entity
    /// mapped to a daily field does it constantly. Carried on the
    /// snapshot so the UI can name the specific source, instead of
    /// leaving the operator to find a warning in a log, which is exactly
    /// how this class of misconfiguration survives for months.
    #[serde(default)]
    pub rain_today_suspect_source: Option<String>,
}

/// Whether a live LOCAL weather station is actually PRESENT for this deployment,
/// the single predicate the station-stale surfaces share so they cannot diverge.
///
/// A station counts as present once it has reported at least one packet
/// (`last_packet_epoch > 0`) OR has identified itself with a serial (non-empty
/// `station_serial`, covering a station that publishes a serial before its first
/// full observation). A cloud-only install (no station ever; only Open-Meteo
/// current) is NOT present.
///
/// Used by BOTH the in-page verdict-strip freshness pill and the /api/health
/// degradation check: a TempestUdp (or any live-station) source that has never
/// produced a packet on a no-station deployment is NOT "offline/degrading" and
/// must not raise a phantom "tempest_lan offline / degraded" banner. Only a
/// station that WAS present and then went quiet is stale. Available in both
/// features (the pill runs on the hydrate side) so the two surfaces share one
/// definition.
pub fn station_present(last_packet_epoch: i64, station_serial: &str) -> bool {
    last_packet_epoch > 0 || !station_serial.is_empty()
}

impl Snapshot {
    /// True once ANY source has produced a current-conditions reading: a
    /// live station packet (`last_packet_epoch`) or a cloud/forecast fill
    /// that claimed the headline (`source_label`). FALSE only in the
    /// boot/empty window, where every numeric field still holds
    /// `Snapshot::default()` zeros; the dashboard renders its warming-up
    /// skeleton there instead of presenting those zeros as readings.
    /// Mirrors `station_present`'s shape; available to both features (the
    /// hydrate-side hero consults it).
    pub fn has_any_reading(&self) -> bool {
        self.last_packet_epoch > 0 || !self.source_label.is_empty()
    }

    /// State-of-charge curve for the Tempest's lithium-titanate (LTO)
    /// battery. Piecewise-linear table copied verbatim from
    /// pyweatherflowudp's calc.py so this app's percentage matches what
    /// HA's WeatherFlow integration shows (and the WeatherFlow help docs
    /// at help.tempest.earth/.../Solar-Power-Rechargeable-Battery).
    /// Charges to 2.80 V; 2.70 is treated as 100% so a slightly degraded
    /// pack still reads "full".
    pub fn battery_pct_from_v(v: f64) -> f64 {
        const CURVE: &[(f64, f64)] = &[
            (2.00, 0.0),
            (2.10, 5.0),
            (2.15, 10.0),
            (2.16, 20.0),
            (2.19, 30.0),
            (2.20, 40.0),
            (2.23, 50.0),
            (2.28, 60.0),
            (2.32, 70.0),
            (2.40, 80.0),
            (2.50, 90.0),
            (2.52, 95.0),
            (2.70, 100.0),
        ];
        if v <= CURVE[0].0 {
            return CURVE[0].1;
        }
        if v >= CURVE[CURVE.len() - 1].0 {
            return CURVE[CURVE.len() - 1].1;
        }
        for w in CURVE.windows(2) {
            let (l, r) = (w[0], w[1]);
            if v >= l.0 && v <= r.0 {
                let slope = (r.1 - l.1) / (r.0 - l.0);
                return l.1 + slope * (v - l.0);
            }
        }
        0.0
    }
}

/// Cap on the recent-strike ring buffer (see apply_strikes for why).
#[cfg(feature = "ssr")]
const MAX_RECENT_STRIKES: usize = 500;

/// Maps the internal snapshot-field provenance key (what `field_provenance` is
/// keyed by) to the canonical WeatherField name the config + UI speak (matching
/// `config::field_overrides::field_name`), for the user-overrideable headline
/// readings. `field_source_map` walks this so the snapshot's `field_sources`
/// keys line up with `field_source_overrides`.
#[cfg(feature = "ssr")]
const PROVENANCE_KEY_TO_FIELD_NAME: &[(&str, &str)] = &[
    ("air_temp_f", "air_temp_f"),
    ("rh_pct", "rh_pct"),
    ("wind_avg_mph", "wind_mph"),
    ("pressure_inhg", "pressure_in_hg"),
    ("rain_in_today", "rain_today_in"),
    ("solar_w_m2", "solar_w_m2"),
    ("uv_index", "uv_index"),
    ("dew_point_f", "dew_point_f"),
];

/// Human display name for a provenance field key, or None to omit it from the
/// conditions-provenance panel (it only surfaces the headline readings).
#[cfg(feature = "ssr")]
fn field_display_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "air_temp_f" => "Air temperature",
        "rh_pct" => "Humidity",
        "wind_avg_mph" => "Wind",
        "pressure_inhg" => "Pressure",
        "rain_in_today" => "Rain",
        "solar_w_m2" => "Solar",
        "uv_index" => "UV index",
        "dew_point_f" => "Dew point",
        "lightning_count_last_min" => "Lightning",
        "leaf_wetness_pct" => "Leaf wetness",
        _ => return None,
    })
}

/// Who currently owns the CURRENT-RAIN field (`rain_intensity_in_hr`) in the
/// merge, surfaced for the refresher's 3-tier honest rain gate. `is_live` is
/// true when a LIVE local station owns the rate freshly (a real gauge: always
/// observation-grade `Measured` rain); false when only the cloud-fill tier owns
/// it. `label` is the owning source's config id (or `TEMPEST_LABEL`) so the
/// refresher can map a cloud owner to its honest rain nature (NWS observation,
/// NOAA MRMS radar QPE, every model provider forecast). `None` from
/// `rain_owner` means no source has written the rain field yet (the gate falls
/// back to the model forecast, nature Model).
#[cfg(feature = "ssr")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RainOwner {
    /// Nature from the booted source kind, never inferred from its configurable id.
    pub nature: crate::model::RainNature,
    /// The owning source's label (config id, or `TEMPEST_LABEL` for the UDP path).
    pub label: String,
    /// True when a LIVE local station owns the rain rate freshly (observation-
    /// grade `Measured`); false when the cloud-fill tier owns it.
    pub is_live: bool,
    /// Whether that owner is still FRESH as of the query instant, judged by the
    /// owner's own `max_age`. A stale cloud owner still names the last cloud that
    /// filled rain; the refresher only surfaces an observation/radar rate while
    /// it is fresh.
    pub is_fresh: bool,
}

/// Slack before a fall in today's rain total counts as a fall.
///
/// A gauge that reports hundredths can jitter in the last place on a
/// unit conversion round trip. Real per-minute rain mapped to a daily
/// field drops by far more than this the moment the minute is dry.
#[cfg(feature = "ssr")]
const RAIN_ACCUM_EPSILON_IN: f64 = 0.005;

/// An accepted current value travels with its own evidence. Transport locality
/// and measurement nature are independent: an NWS observation is measured,
/// while a model's current interval remains an estimate.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CurrentWeatherSample {
    pub value: f64,
    pub source_id: String,
    pub observed_epoch: i64,
    pub max_age_s: i64,
    pub measured: bool,
    #[serde(default)]
    pub selection_reason: String,
}

impl CurrentWeatherSample {
    pub fn summary_at(&self, now: i64) -> String {
        let age = now.saturating_sub(self.observed_epoch);
        let time = if self.observed_epoch <= 0 || self.observed_epoch > now {
            "report time unknown".into()
        } else if age > self.max_age_s {
            format!("stale · {}m old", age / 60)
        } else if age < 60 {
            "just reported".into()
        } else {
            format!("{}m old", age / 60)
        };
        format!(
            "{} · {} · {time}",
            self.source_id,
            if self.measured {
                "measured"
            } else {
                "estimated"
            }
        )
    }
}

#[cfg(feature = "ssr")]
pub struct LiveWeatherStore {
    /// Written by the snapshot bridge alone (one task, so writes never
    /// race), read lock-free by everyone. A test exercises the writers
    /// directly; nothing else does.
    current: ArcSwap<Snapshot>,
    tx: watch::Sender<Arc<Snapshot>>,
    rx: watch::Receiver<Arc<Snapshot>>,
    rolling: Mutex<RollingBuffers>,
    /// Per-source current-conditions priority, keyed by the source's config
    /// id (the bus source_id). Set at startup from config and on hot-reload
    /// so the arbiter can rank live sources. Lock-free reads on the hot path.
    pub(super) priorities: ArcSwap<HashMap<String, i32>>,
    /// Per-source MAX-AGE (seconds), keyed by source id like `priorities`.
    /// The freshness window the arbiter judges a source's OWNED
    /// fields by: a source is "still fresh" for `max_age` seconds after its last
    /// write. Set from `config.sources[*].max_age_s` at boot + on hot-reload, so
    /// a slow cloud cadence (Open-Meteo / NWS / Met.no ~1800s) is honored instead
    /// of the one hardcoded 600s window. Lock-free reads on the hot path; an
    /// unlisted source (or one with no configured max_age) falls back to
    /// `LIVE_FRESHNESS_SECS` via `max_age_for`.
    pub(super) max_ages: ArcSwap<HashMap<String, i32>>,
    pub(super) rain_natures: ArcSwap<HashMap<String, crate::model::RainNature>>,
    observed_condition_fields: ArcSwap<HashMap<String, [bool; 3]>>,
    current_samples:
        Mutex<HashMap<crate::ports::weather_source::WeatherField, CurrentWeatherSample>>,
    /// PER-FIELD ownership: snapshot-field key -> (owning live source's priority,
    /// epoch it last wrote, owning source label). The arbiter consults this so
    /// each field comes from its highest-priority fresh source, and a partial
    /// source only owns the fields it actually provides. The label lets the
    /// established owner refresh its OWN field at equal priority (otherwise the
    /// strict-`>` rule would block a single source from updating its own reading).
    ///
    /// INVARIANT (ownership follows the writer): an entry here means a LIVE
    /// source wrote that field LAST. When a cloud write takes a field, including
    /// a pinned/chained cloud taking it from a still-fresh live owner,
    /// `apply_source_fields` REMOVES the live entry and records the cloud in
    /// `fill_owners` instead. So a reader that finds a fresh entry here is
    /// looking at the source that actually produced the value, and never at a
    /// station whose reading a cloud has since replaced.
    pub(super) field_owners: Mutex<HashMap<&'static str, (i32, i64, String)>>,
    /// PER-FIELD CLOUD-FILL ownership, the cloud-only fallback chain:
    /// snapshot-field key -> (filling cloud's priority, epoch it last filled,
    /// cloud label). SEPARATE from `field_owners` and from the live tier: a
    /// forecast/cloud source competes here by priority so a higher-priority cloud
    /// wins the fill and a cloud that goes stale past its `max_age` DEMOTES to the
    /// next-highest cloud, instead of "last writer wins" (the prior staleness-only
    /// fill let any cloud overwrite the display). Never sets `has_live_station`,
    /// the live epochs, or `last_packet_epoch` (a cloud fill is not a station).
    pub(super) fill_owners: Mutex<HashMap<&'static str, (i32, i64, String)>>,
    /// Accepted flow facts carry value and provenance together. Unlike the v1
    /// raw scalar defaults, absence here never masquerades as an idle meter.
    flow_samples: Mutex<(
        Option<crate::model::flow::FlowSample>,
        Option<crate::model::flow::FlowSample>,
    )>,
    /// PER-FIELD provenance for display: snapshot-field key -> the human source
    /// label that last wrote it (a live claim or a forecast fill). Powers the
    /// "which source drives each reading" panel. Separate from field_owners (and
    /// locked independently, never nested) so the display map never perturbs the
    /// hot arbitration path.
    field_provenance: Mutex<HashMap<&'static str, String>>,
    /// PER-FIELD user overrides: snapshot-field key -> the writer LABEL the
    /// operator pinned to own that field (its config id), installed from
    /// `config.field_source_overrides`. Empty (the
    /// default) means no override -> the priority arbitration below is unchanged,
    /// so a deployment that never sets one merges byte-identically. Lock-free
    /// reads on the hot per-packet path (arc-swapped at install only).
    pub(super) field_overrides: ArcSwap<HashMap<&'static str, String>>,
    /// Freshness witness for the override/chain decision: (snapshot-field key,
    /// writer LABEL) -> (the epoch that CHAIN-ENTRY source last wrote that field,
    /// whether that source is a LIVE source). The arbiter records both whenever a
    /// chain entry writes its own field, and reads them (per chain entry) to decide
    /// which entry is the FIRST currently-fresh owner (so a later or off-chain
    /// source is blocked) or whether the whole chain is stale. Keyed by (field,
    /// label) rather than field alone so a multi-entry chain witnesses each entry's
    /// freshness independently (a single pin is just a 1-entry chain, so it keys one
    /// pair and behaves byte-identically). The is-live flag drives the TIER LOCK
    ///: when the fresh chain owner is a CLOUD that then goes stale, a live
    /// station must NOT reclaim the field while some cloud still fills it (it demotes
    /// through the cloud fill chain instead); when the chain owner is a LIVE station
    /// that goes stale, the field falls back to the live priority merge so it is
    /// never lost. Only touched for keys that have a chain/override, so a plain
    /// deployment never locks it.
    pub(super) field_override_seen: Mutex<HashMap<(&'static str, String), (i64, bool)>>,
    /// PER-FIELD user PRIORITY CHAINS: snapshot-field key -> the ORDERED list of
    /// writer LABELs the operator wants to own that field, primary first
    /// (its config id), installed from
    /// `config.field_source_chains`. The ordered-failover generalization of
    /// `field_overrides`: the FIRST label in the list that is currently FRESH owns
    /// the field, and if it goes quiet the next takes over. Empty (the default)
    /// means no chain -> `field_overrides` (a single pin) is consulted instead, and
    /// if that is empty too the priority arbitration below is unchanged, so a
    /// deployment that never sets one merges byte-identically. A ONE-element chain
    /// behaves byte-for-byte like the equivalent single pin. Lock-free reads on the
    /// hot per-packet path (arc-swapped at install only).
    pub(super) field_chains: ArcSwap<HashMap<&'static str, Vec<String>>>,
}

#[cfg(feature = "ssr")]
#[derive(Default)]
struct RollingBuffers {
    pressure: VecDeque<(i64, f64)>, // last 6h of pressure samples
    strikes: VecDeque<StrikeEvent>, // last hour of strikes
    // Independent totals prevent a backup gauge's minutes entering the
    // primary's total. A source/epoch is integrated at most once.
    rain_by_source: HashMap<String, RainAccumulator>,
    // LOCAL calendar day of the last et0_today write (apply_source_fields).
    // 0 = never written through that path (unknown day, e.g. a demo store()
    // seed): the carry-forward then leaves the value alone. Any other day
    // mismatch zeroes the carried accumulator at rollover, mirroring
    // rain_today_day, so a source quiet across midnight can't pin yesterday's
    // total into the new day.
    et0_today_day: i32,
}

#[cfg(feature = "ssr")]
struct RainAccumulator {
    day: i32,
    total_mm: f64,
    last_epoch: i64,
}

/// Local calendar-day ordinal for a UNIX epoch: num_days_from_ce of the local
/// date in the deployment's CONFIGURED timezone (crate::timeutil), so the
/// rain-today / et0-today day buckets roll over at the DEPLOYMENT's midnight.
/// The previous chrono::Local frame was the CONTAINER's timezone: in a UTC
/// container (the common public deployment) it reset the accumulators at UTC
/// midnight, mid-evening for any US tz, and split overnight storms across two
/// "days". Falls back to the integer UTC day for an unrepresentable epoch.
#[cfg(feature = "ssr")]
fn local_day_ordinal(epoch: i64) -> i32 {
    crate::timeutil::local_day_ordinal(epoch)
}

#[cfg(feature = "ssr")]
impl Default for LiveWeatherStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "ssr")]
impl LiveWeatherStore {
    pub fn new() -> Self {
        let initial = Arc::new(Snapshot::default());
        let (tx, rx) = watch::channel(initial.clone());
        Self {
            current: ArcSwap::from(initial),
            tx,
            rx,
            rolling: Mutex::new(RollingBuffers::default()),
            priorities: ArcSwap::from(Arc::new(HashMap::new())),
            max_ages: ArcSwap::from(Arc::new(HashMap::new())),
            rain_natures: ArcSwap::from(Arc::new(HashMap::new())),
            observed_condition_fields: ArcSwap::from(Arc::new(HashMap::new())),
            current_samples: Mutex::new(HashMap::new()),
            field_owners: Mutex::new(HashMap::new()),
            fill_owners: Mutex::new(HashMap::new()),
            flow_samples: Mutex::new((None, None)),
            field_provenance: Mutex::new(HashMap::new()),
            field_overrides: ArcSwap::from(Arc::new(HashMap::new())),
            field_override_seen: Mutex::new(HashMap::new()),
            field_chains: ArcSwap::from(Arc::new(HashMap::new())),
        }
    }

    /// Per-field current-conditions provenance keyed by the canonical
    /// WeatherField name (e.g. `wind_mph -> "tempest"`), for the snapshot's
    /// `field_sources` map + the per-field source picker. Reads the same live
    /// `field_provenance` ownership the panel does, but maps the internal
    /// snapshot-field key to the WeatherField name the config + UI speak, so a
    /// reading the user can override ("Wind") is keyed the same here as in
    /// `field_source_overrides`. Only user-overrideable scalar fields are
    /// included; a field no source has written yet is simply absent.
    pub fn field_source_map(&self) -> std::collections::BTreeMap<String, String> {
        let prov = self.field_provenance.lock().unwrap();
        let mut out = std::collections::BTreeMap::new();
        for (snap_key, name) in PROVENANCE_KEY_TO_FIELD_NAME {
            if let Some(src) = prov.get(*snap_key) {
                out.insert((*name).to_string(), src.clone());
            }
        }
        out
    }

    pub fn flow_readout(&self, now: i64) -> crate::model::FlowReadout {
        let samples = self.flow_samples.lock().unwrap();
        let read = |sample: &Option<crate::model::flow::FlowSample>| {
            sample.as_ref().and_then(|s| {
                s.fresh_value(now, self.max_age_for(&s.source_id))
                    .map(|value| (value, format!("source:{}", s.source_id)))
            })
        };
        let rate = read(&samples.0);
        let total = samples
            .1
            .as_ref()
            .filter(|s| local_day_ordinal(s.observed_epoch) == local_day_ordinal(now))
            .and_then(|_| read(&samples.1));
        crate::model::FlowReadout {
            rate_gpm: rate.as_ref().map(|v| v.0),
            rate_source_id: rate.map(|v| v.1),
            total_gal_today: total.as_ref().map(|v| v.0),
            total_source_id: total.map(|v| v.1),
        }
    }

    /// A disconnect invalidates live meter evidence immediately; reconnect
    /// requires a new observation before a number is published again.
    pub fn invalidate_flow_source(&self, source_id: &str) {
        // Match the writer's lock order. Release only flow claims/witnesses;
        // temperature, rain and the source's other last-known facts survive.
        let mut owners = self.field_owners.lock().unwrap();
        let mut fills = self.fill_owners.lock().unwrap();
        for key in ["flow_gpm", "flow_total_gal_today"] {
            if owners.get(key).is_some_and(|s| s.2 == source_id) {
                owners.remove(key);
            }
            if fills.get(key).is_some_and(|s| s.2 == source_id) {
                fills.remove(key);
            }
        }
        self.field_override_seen
            .lock()
            .unwrap()
            .retain(|(key, label), _| {
                label != source_id || !matches!(*key, "flow_gpm" | "flow_total_gal_today")
            });
        let mut samples = self.flow_samples.lock().unwrap();
        if samples.0.as_ref().is_some_and(|s| s.source_id == source_id) {
            samples.0 = None;
        }
        if samples.1.as_ref().is_some_and(|s| s.source_id == source_id) {
            samples.1 = None;
        }
    }

    /// The COMPLETE set of writer labels currently attributed an owned field by
    /// the merge: every distinct value in `field_provenance`, across ALL fields,
    /// not just the user-overrideable headline subset `field_source_map` maps. A
    /// source that owns only a NON-headline field (e.g. an Ecowitt gateway that
    /// owns soil moisture, which is not in `PROVENANCE_KEY_TO_FIELD_NAME`) is
    /// present here even though it is absent from `field_source_map`.
    ///
    /// The honest-status taxonomy tests a source's WRITER LABEL (`TEMPEST_LABEL`
    /// for the UDP path, the config id otherwise, the SAME label the merge stamps)
    /// for membership in this set to decide `owns_field`. Matching on the raw
    /// writer label (not a friendly display name) keeps it congruent with what the
    /// merge actually wrote, and the complete field coverage stops a soil-only (or
    /// any non-headline) owner from mis-reading `falling_through`. Reads the same
    /// `field_provenance` ownership `field_source_map` does, so the two never drift.
    pub fn current_owner_labels(&self) -> std::collections::BTreeSet<String> {
        let prov = self.field_provenance.lock().unwrap();
        prov.values().cloned().collect()
    }

    /// Per-field current-conditions provenance for the UI: (display name, source
    /// label) in a stable display order, for the headline readings a source is
    /// currently providing. Lets the operator see exactly which source drives
    /// each reading (e.g. temp/wind from Tempest, pressure from an Ecowitt GW).
    pub fn conditions_provenance(&self) -> Vec<(&'static str, String)> {
        const ORDER: [&str; 10] = [
            "air_temp_f",
            "rh_pct",
            "wind_avg_mph",
            "pressure_inhg",
            "rain_in_today",
            "solar_w_m2",
            "uv_index",
            "dew_point_f",
            "lightning_count_last_min",
            "leaf_wetness_pct",
        ];
        let prov = self.field_provenance.lock().unwrap();
        ORDER
            .iter()
            .filter_map(|k| {
                let src = prov.get(*k)?;
                let name = field_display_name(k)?;
                Some((name, src.clone()))
            })
            .collect()
    }

    /// Install cloud source nature by actual configured kind before sources start.
    pub fn set_rain_natures(&self, map: HashMap<String, crate::model::RainNature>) {
        self.rain_natures.store(Arc::new(map));
    }

    fn rain_field_owner(&self, key: &'static str, at: i64) -> Option<RainOwner> {
        let make = |epoch: i64, label: &String, live: bool| RainOwner {
            label: label.clone(),
            is_live: live,
            is_fresh: !owner_is_stale(self.max_age_for_field(label, key), epoch, at),
            nature: self
                .rain_natures
                .load()
                .get(label)
                .copied()
                .unwrap_or(if live {
                    crate::model::RainNature::Measured
                } else {
                    crate::model::RainNature::Model
                }),
        };
        // A stale live writer still owns the stored scalar. A historical fill
        // cannot certify that scalar; an actual cloud takeover removes this owner.
        if let Some((_, epoch, label)) = self.field_owners.lock().unwrap().get(key) {
            return Some(make(*epoch, label, true));
        }
        self.fill_owners
            .lock()
            .unwrap()
            .get(key)
            .map(|(_, epoch, label)| make(*epoch, label, false))
    }

    /// Owner and freshness travel with the current rain rate.
    pub fn rain_owner(&self, at: i64) -> Option<RainOwner> {
        self.rain_field_owner("rain_intensity_in_hr", at)
    }

    /// Owner and freshness travel with the accumulated daily rain.
    pub fn rain_today_owner(&self, at: i64) -> Option<RainOwner> {
        self.rain_field_owner("rain_in_today", at)
    }

    /// True when the ET0-today field is currently owned by a FRESH LIVE source
    /// (judged by that source's own max_age, as of `at`). Live stations report
    /// ET0 as an accumulator since local midnight, while a cloud fill (e.g. the
    /// Open-Meteo current emit) writes the FULL-DAY forecast figure; the
    /// refresher uses this ownership bit, never a magnitude heuristic, to
    /// decide whether the bus `et0_today` can honestly serve as
    /// eto_spent_today_mm. Only reads `field_owners` (live claims); a fill
    /// never registers there.
    pub fn et0_today_is_live(&self, at: i64) -> bool {
        let owners = self.field_owners.lock().unwrap();
        owners.get("et0_today").is_some_and(|(_, oe, ol)| {
            !owner_is_stale(self.max_age_for_field(ol, "et0_today"), *oe, at)
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    pub fn set_observed_condition_fields(&self, sources: HashMap<String, [bool; 3]>) {
        self.observed_condition_fields.store(Arc::new(sources));
    }

    pub fn current_weather_samples(
        &self,
        now: i64,
    ) -> std::collections::BTreeMap<String, CurrentWeatherSample> {
        let samples = self.current_samples.lock().unwrap().clone();
        samples
            .into_iter()
            .filter_map(|(field, mut sample)| {
                let key = field_owner_key(field)?;
                sample.max_age_s = self.max_age_for_field(&sample.source_id, key);
                sample.selection_reason = match self
                    .field_chain_for(key)
                    .and_then(|chain| chain.first().cloned())
                {
                    Some(primary) if primary == sample.source_id => "Preferred source".into(),
                    Some(primary) => {
                        let seen = self.field_override_seen.lock().unwrap();
                        match seen.get(&(key, primary.clone())) {
                            Some((epoch, _))
                                if owner_is_stale(
                                    self.max_age_for_field(&primary, key),
                                    *epoch,
                                    now,
                                ) =>
                            {
                                format!("{primary} report expired; using fallback")
                            }
                            _ => format!("{primary} has no usable report; using fallback"),
                        }
                    }
                    None => "Automatic source selection".into(),
                };
                Some((field.name().to_string(), sample))
            })
            .collect()
    }

    #[cfg(test)]
    pub fn current_condition_samples(&self, now: i64) -> [Option<CurrentWeatherSample>; 3] {
        let samples = self.current_weather_samples(now);
        ["air_temp_f", "wind_mph", "rh_pct"].map(|field| samples.get(field).cloned())
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.rx.clone()
    }

    /// Add one reported minute of rain to the day's total, in mm, and
    /// answer with the total. A station that reports the increment rather
    /// than a since-midnight figure is the only producer; the day bucket
    /// is the DEPLOYMENT's calendar, so an overnight storm is one day's
    /// rain rather than two.
    fn accumulate_rain_mm(&self, source: &str, last_min_mm: f64, at_epoch: i64) -> Option<f64> {
        if !last_min_mm.is_finite() || last_min_mm < 0.0 {
            return None;
        }
        let bucket = local_day_ordinal(at_epoch);
        let mut roll = self.rolling.lock().unwrap();
        let acc = roll
            .rain_by_source
            .entry(source.to_string())
            .or_insert(RainAccumulator {
                day: bucket,
                total_mm: 0.0,
                last_epoch: i64::MIN,
            });
        // UDP may repeat or reorder an observation, including one crossing
        // midnight. Neither can rewind the bucket or charge rain twice.
        if at_epoch < acc.last_epoch {
            return None;
        }
        if at_epoch == acc.last_epoch {
            return Some(acc.total_mm);
        }
        if acc.day != bucket {
            acc.day = bucket;
            acc.total_mm = 0.0;
        }
        acc.last_epoch = at_epoch;
        acc.total_mm += last_min_mm;
        Some(acc.total_mm)
    }

    /// Restore one source's unique minute reports before producers start.
    /// The last report identity prevents a polled HA state from being counted
    /// again after restart. Zero is a valid dry-day checkpoint.
    pub fn restore_rain_minutes(
        &self,
        source: &str,
        rain_in: f64,
        last_epoch: i64,
        now: i64,
    ) -> bool {
        let bucket = local_day_ordinal(last_epoch);
        if last_epoch <= 0
            || last_epoch > now
            || bucket != local_day_ordinal(now)
            || !rain_in.is_finite()
            || rain_in < 0.0
        {
            return false;
        }
        let mut roll = self.rolling.lock().unwrap();
        if roll.rain_by_source.contains_key(source) {
            return false;
        }
        roll.rain_by_source.insert(
            source.into(),
            RainAccumulator {
                day: bucket,
                total_mm: crate::units::in_to_mm(rain_in),
                last_epoch,
            },
        );
        true
    }

    /// Replace the snapshot wholesale. Used by demo-mode synthesis to
    /// drop synthetic data into the live store without going through
    /// the bus. Real readings arrive through `apply_source_fields` so the
    /// rolling buffers and the ownership maps stay accurate.
    pub fn store(&self, snap: Snapshot) {
        let arc = Arc::new(snap);
        self.current.store(arc.clone());
        let _ = self.tx.send(arc);
    }

    /// Apply a batch of bus weather fields from one source, carrying every
    /// other field forward. The one write path for readings: the Tempest
    /// on the LAN, an Ecowitt gateway, a Davis console and a cloud API all
    /// arrive here through the snapshot bridge.
    ///
    /// Arbitration is PER FIELD (see field_owner_key / live_claim_field):
    /// - A live source (live_current=true) claims a field if its priority >= the
    ///   field's current owner (or the owner is stale), sets the value, and
    ///   records ownership + the field's live epoch. It also stamps
    ///   `last_packet_epoch` (whole-snapshot freshness) for any field it writes.
    /// - A forecast source (live_current=false) FILLS a field when no live source
    ///   owns it freshly, or when an operator's pin/chain hands it the field
    ///   outright (see override_decision). It records itself in `fill_owners`, not
    ///   in `field_owners`, and CLEARS any live owner it displaced there, so the
    ///   live tier never names a station for a value a cloud wrote. It never
    ///   stamps `last_packet_epoch` or the per-field live epochs -- so the
    ///   selected-current samples separately retain measurement nature, so the
    ///   engine distinguishes remote observations from model estimates.
    #[cfg(test)]
    pub fn apply_source_fields(
        &self,
        fields: &[(crate::ports::weather_source::WeatherField, f64)],
        at_epoch: i64,
        live_current: bool,
        source_label: &str,
    ) {
        // Historical test replay supplies its own arrival clock.
        self.apply_received_fields(fields, at_epoch, at_epoch, live_current, source_label);
    }

    /// Admit a bus observation using report time for evidence and arrival time
    /// for freshness. An old, repeatedly polled value cannot reclaim a fresh
    /// fallback, even when the old source has a higher priority or a pin.
    pub fn apply_received_fields(
        &self,
        fields: &[(crate::ports::weather_source::WeatherField, f64)],
        at_epoch: i64,
        received_epoch: i64,
        live_current: bool,
        source_label: &str,
    ) {
        use crate::ports::weather_source::WeatherField as F;
        let current_fields: Vec<_> = fields
            .iter()
            .copied()
            .filter(|(field, _)| {
                field_owner_key(*field).is_some_and(|key| {
                    !owner_is_stale(
                        self.max_age_for_field(source_label, key),
                        at_epoch,
                        received_epoch,
                    )
                })
            })
            .collect();
        let fields = current_fields.as_slice();
        if fields.is_empty() {
            return;
        }
        // Integrated before any ownership lock is taken, so the loop below
        // can offer the day's total to the arbiter as this source's
        // rain_in_today. Same lock order as everything else here: rolling
        // is never held across the owners maps.
        let last_min_in = fields
            .iter()
            .find_map(|(f, v)| matches!(f, F::RainLastMinIn).then_some(*v));
        let accumulated_today_in = last_min_in.and_then(|v| {
            self.accumulate_rain_mm(source_label, crate::units::in_to_mm(v), at_epoch)
                .map(crate::units::mm_to_in)
        });
        let prev = self.current.load_full();
        // One configuration view for every field and both sides of each
        // comparison, even if priorities hot-reload while this packet applies.
        let priorities = self.priorities.load();
        let prio = Self::priority_in(&priorities, source_label);
        let mut snap = (*prev).clone();
        let mut owners = self.field_owners.lock().unwrap();
        // Cloud-fill chain. Locked right after `field_owners` so the two
        // locks always nest in the SAME order (owners -> fills). A forecast/
        // cloud write competes here by priority + max_age; a live write never
        // touches it.
        let mut fills = self.fill_owners.lock().unwrap();
        let mut touched = false;
        // Set the moment a LIVE source (live_current=true) actually claims a
        // field: that is what makes `has_live_station` true (a forecast fill
        // does not). One live claim of any current-conditions field means a
        // real station is present and producing.
        let mut live_claimed = false;
        let mut owns_air_temp = false;
        let mut owns_wind = false;
        let mut owns_rh = false;
        let mut owns_rain = false;
        let mut wrote_et0 = false;
        let mut wrote_pressure: Option<f64> = None;
        // Keys this source wrote this call -> recorded as provenance after the
        // owners lock drops (a live claim or a forecast fill both count).
        let mut prov_keys: Vec<&'static str> = Vec::new();
        for (field, value) in fields {
            let v = *value;
            if !v.is_finite()
                || (matches!(
                    *field,
                    F::WindMph | F::WindGustMph | F::WindLullMph | F::RapidWindMph
                ) && v < 0.0)
                || (*field == F::RhPct && !(0.0..=100.0).contains(&v))
                || (matches!(
                    *field,
                    F::FlowGpm
                        | F::FlowTotalGalToday
                        | F::RainTodayIn
                        | F::RainIntensityInHr
                        | F::RainLastMinIn
                ) && v < 0.0)
                || (*field == F::RainLastMinIn && accumulated_today_in.is_none())
            {
                continue;
            }
            // PER-FIELD arbitration. A live source claims a field by priority; a
            // forecast source only FILLS a field no live source owns freshly. A
            // partial source (e.g. a soil gateway with only a barometer) thus
            // owns ONLY the fields it provides and can never zero out the
            // temp/RH/wind another live station owns.
            let Some(key) = field_owner_key(*field) else {
                continue; // string / structured-forecast variants: not this path
            };
            // PER-FIELD USER OVERRIDE (additive): if the operator pinned a source
            // to this field, that decision precedes priority. Some(true) forces
            // this writer to win; Some(false) blocks it (the pinned source is the
            // fresh owner); None means no override / the pinned source is stale,
            // so we fall through to the unchanged priority arbitration below.
            // Last-resort backup signal for the tier lock: is some cloud still
            // freshly filling this field? Read directly from the already-held
            // `fills` lock (no re-lock, so the owners -> fills order is preserved).
            let cloud_tier_fresh = self.cloud_tier_fresh(&fills, key, received_epoch);
            // Who owned this field BEFORE this write. Arbitration below
            // rewrites the owners map (it inserts on a live claim and CLEARS
            // the entry when a cloud takes the field), so anything that wants
            // to know whether a writer is updating its own field has to look
            // now. This is the LIVE tier only: `None` means either nobody has
            // written the field or a cloud currently provides it, and the
            // rain-accumulator guard below deliberately treats both as "not
            // the same writer" -- a fall across an ownership change is
            // failover, not the per-minute-entity misconfiguration it hunts.
            let prior_owner: Option<String> = owners.get(key).map(|o| o.2.clone());
            let allowed = match self.override_decision(
                key,
                source_label,
                at_epoch,
                received_epoch,
                live_current,
                cloud_tier_fresh,
            ) {
                Some(true) => {
                    // Force ownership so the field reads as live/owned and a
                    // later same-or-lower priority source can't steal it within
                    // the freshness window; mirrors a normal live claim. A pinned
                    // CLOUD records in the cloud chain instead (it is not a live
                    // owner) so its own demote-on-stale is tracked there, and the
                    // live entry it displaced is cleared below (ownership follows
                    // the writer) so no live station stays recorded as the owner
                    // of a number a cloud wrote.
                    if live_current {
                        owners.insert(key, (prio, at_epoch, source_label.to_string()));
                    } else {
                        fills.insert(key, (prio, at_epoch, source_label.to_string()));
                    }
                    true
                }
                Some(false) => false,
                None => {
                    if live_current {
                        self.live_claim_field(
                            &mut owners,
                            key,
                            &priorities,
                            at_epoch,
                            received_epoch,
                            source_label,
                        )
                    } else {
                        // CLOUD FILL, priority-aware: only when no fresh
                        // LIVE station owns the field (forecast_may_fill), AND this
                        // cloud wins the cloud chain over any fresher higher cloud
                        // (cloud_fill_field, which also demotes a cloud that went
                        // stale past its max_age to the next-highest). Both gates
                        // must pass: the live gate protects a real station, the
                        // cloud gate ranks the clouds among themselves.
                        self.forecast_may_fill(&owners, key, received_epoch)
                            && self.cloud_fill_field(
                                &mut fills,
                                key,
                                &priorities,
                                at_epoch,
                                received_epoch,
                                source_label,
                            )
                    }
                }
            };
            if !allowed {
                continue;
            }
            // OWNERSHIP FOLLOWS THE WRITER. This write takes the field, so the
            // owner recorded for the field has to be the source that wrote it.
            // The LIVE path already did that (the pin branch inserts; the
            // priority merge inserts on a claim). A CLOUD write records itself in
            // `fills` instead, so the LIVE entry it displaced has to go: the live
            // tier no longer provides this field.
            //
            // Without the clear, a pinned or chained cloud could take a field out
            // from under a STILL-FRESH live owner (`override_decision` -> Some(true)
            // bypasses `forecast_may_fill` on purpose, that is what a pin is) and
            // leave the live station recorded as the owner. Then `rain_today_owner`
            // / `rain_owner` / `et0_today_is_live`, which all read the live tier
            // first and return early while it looks fresh, answered "a fresh live
            // station" for a number the cloud wrote: a MODEL day total filed as
            // gauge-measured under the station's name, which the observations
            // ledger then day-maxes and ranks above the model rain archive for the
            // whole balance window. The rain gate cannot tell measured from
            // modelled if the owner map lies.
            //
            // Clearing (rather than writing the cloud's label into `field_owners`)
            // is what keeps the two tiers honest: `field_owners` holds LIVE claims
            // only, and the cloud's ownership lives in `fills`, where the two-tier
            // readers report it with `is_live=false` and its own freshness.
            if !live_current {
                owners.remove(key);
            }
            // A live source is writing a current-conditions field this call:
            // a real station is present and producing. A forecast fill
            // (live_current=false) writes for display but never sets this.
            if live_current {
                live_claimed = true;
            }
            let condition_index = match field {
                F::AirTempF => Some(0),
                F::WindMph
                | F::WindGustMph
                | F::WindLullMph
                | F::RapidWindMph
                | F::WindBearingDeg
                | F::RapidWindBearingDeg => Some(1),
                F::RhPct => Some(2),
                _ => None,
            };
            if let Some(index) = condition_index {
                let measured = live_current
                    || self
                        .observed_condition_fields
                        .load()
                        .get(source_label)
                        .is_some_and(|fields| fields[index]);
                self.current_samples.lock().unwrap().insert(
                    *field,
                    CurrentWeatherSample {
                        value: v,
                        source_id: source_label.into(),
                        observed_epoch: at_epoch,
                        max_age_s: self.max_age_for(source_label),
                        measured,
                        selection_reason: String::new(),
                    },
                );
            }
            match field {
                F::AirTempF => {
                    snap.air_temp_f = v;
                    owns_air_temp = true;
                }
                F::DewPointF => snap.dew_point_f = v,
                F::RhPct => {
                    snap.rh_pct = v;
                    owns_rh = true;
                }
                F::WindMph => {
                    snap.wind_avg_mph = v;
                    owns_wind = true;
                }
                F::WindGustMph => snap.wind_gust_mph = v,
                F::WindBearingDeg => snap.wind_dir_deg = v,
                F::PressureInHg => {
                    snap.pressure_inhg = v;
                    wrote_pressure = Some(v);
                }
                F::SolarWm2 => snap.solar_w_m2 = v,
                F::UvIndex => snap.uv_index = v,
                F::Illuminance => snap.illuminance_lx = v,
                F::RainTodayIn => {
                    // Rain today is a SINCE-MIDNIGHT ACCUMULATION, and
                    // this path takes the number verbatim. The Tempest
                    // UDP path integrates per-minute deltas itself; a bus
                    // source is trusted to have done that already.
                    //
                    // That trust is easy to break by accident. Home
                    // Assistant's WeatherFlow integration exposes a
                    // `precipitation` entity that reports rain in the
                    // last reporting MINUTE, and mapping it here is a
                    // natural mistake: the names match, and it produces
                    // plausible small numbers.
                    //
                    // The damage is not confined to the reading. A live
                    // owner's rain_today_in is classified as measured
                    // gauge data and written into the observations
                    // ledger, which takes the day's MAXIMUM and outranks
                    // the model rain archive for the whole balance
                    // window. One wet day recorded as its heaviest single
                    // minute suppresses correct model rain for a week,
                    // and the already-wet skip reads near zero the
                    // morning after a soaking.
                    //
                    // A real accumulator only rises within a local day.
                    // A per-minute value oscillates. So a DROP inside the
                    // same local day, from a source that is not simply
                    // re-reporting a reset, is evidence the mapping is
                    // wrong, and the safe move is to keep the higher
                    // number and say so rather than record the dip.
                    // Only when the SAME source's own total falls.
                    //
                    // A fall across an ownership change is normal and
                    // correct: when a primary goes stale the next source
                    // in the chain takes over with its own, quite
                    // possibly lower, number. That is failover working.
                    // A per-minute entity mapped to a daily field is a
                    // different signature entirely: the SAME writer
                    // reporting a total that goes down and up all day.
                    let same_owner = prior_owner.as_deref() == Some(source_label);
                    let same_day = snap.rain_today_day_ordinal == local_day_ordinal(at_epoch);
                    if same_owner && same_day && v + RAIN_ACCUM_EPSILON_IN < snap.rain_in_today {
                        tracing::warn!(
                            source = source_label,
                            reported_in = v,
                            held_in = snap.rain_in_today,
                            "rain today went DOWN within the same day; keeping the higher                              total. Rain today must be a since-midnight accumulation, not                              per-minute rain. Check which entity is mapped to it."
                        );
                        snap.rain_today_suspect_source = Some(source_label.to_string());
                    } else {
                        snap.rain_in_today = v;
                        // The fill's own valid time decides which local day
                        // the total belongs to: the ledger's midnight gate
                        // reads this, so a fill valid late yesterday
                        // arriving after midnight is never recorded on the
                        // new day's row.
                        snap.rain_today_day_ordinal = local_day_ordinal(at_epoch);
                        if !same_day {
                            snap.rain_today_suspect_source = None;
                        }
                    }
                }
                F::RainIntensityInHr => {
                    snap.rain_intensity_in_hr = v;
                    owns_rain = true;
                }
                F::RainLastMinIn => {
                    // The minute and its integrated total must have the
                    // same owner; a backup gauge cannot overwrite either.
                    if let Some(total) = accumulated_today_in {
                        snap.rain_in_last_min = v;
                        snap.rain_in_today = total;
                        snap.rain_today_day_ordinal = local_day_ordinal(at_epoch);
                    }
                }
                F::LightningCount => {
                    snap.lightning_count_last_min = v.max(0.0) as u32;
                    // A minute that detected nothing has no average
                    // distance. Zero on a distance channel reads as
                    // "overhead", so a quiet minute must publish no
                    // reading rather than a number, and a source reporting
                    // a quiet minute sends the count without a distance.
                    if snap.lightning_count_last_min == 0 {
                        snap.lightning_avg_dist_mi = None;
                    }
                }
                // Same no-reading convention as the Tempest packet: every
                // station kind reports a bare 0 for a quiet interval, and 0
                // miles on a distance channel means "overhead", not "none".
                F::LightningDistanceMi => {
                    snap.lightning_avg_dist_mi = (v > 0.0).then_some(v);
                }
                F::Et0Today => {
                    snap.et0_today = v;
                    wrote_et0 = true;
                }
                F::FlowGpm => {
                    snap.flow_gpm = v;
                    self.flow_samples.lock().unwrap().0 = Some(crate::model::flow::FlowSample {
                        value: v,
                        source_id: source_label.to_string(),
                        observed_epoch: at_epoch,
                    });
                }
                F::FlowTotalGalToday => {
                    snap.flow_total_gal_today = v;
                    self.flow_samples.lock().unwrap().1 = Some(crate::model::flow::FlowSample {
                        value: v,
                        source_id: source_label.to_string(),
                        observed_epoch: at_epoch,
                    });
                }
                F::Pop => snap.pop_pct = Some(v),
                F::LeafWetness => snap.leaf_wetness_pct = Some(v),
                F::WindLullMph => snap.wind_lull_mph = v,
                F::RapidWindMph => snap.rapid_wind_mph = v,
                F::RapidWindBearingDeg => snap.rapid_wind_dir = v,
                F::BatteryV => {
                    snap.battery_v = v;
                    snap.battery_pct = Snapshot::battery_pct_from_v(v);
                }
                F::PrecipType => snap.precip_type = v.clamp(0.0, 255.0) as u8,
                F::RainTypeStr | F::ForecastDaily | F::ForecastHourly => continue,
            }
            prov_keys.push(key);
            touched = true;
        }
        drop(owners);
        drop(fills);
        if !touched {
            return;
        }
        // Day-bucket the ET0 accumulator like rain_in_today: an Et0Today write
        // stamps the local day it belongs to; a value carried forward from a
        // PREVIOUS local day is zeroed, so a source that reported ET0 yesterday
        // and went quiet across midnight cannot pin yesterday's total into the
        // new day (issue #4). rolling nests inside write_gate (same order as
        // apply_obs) and is only taken after owners/fills drop.
        {
            let mut roll = self.rolling.lock().unwrap();
            let day = local_day_ordinal(at_epoch);
            if wrote_et0 {
                roll.et0_today_day = day;
            } else if roll.et0_today_day != 0 && roll.et0_today_day != day && snap.et0_today != 0.0
            {
                snap.et0_today = 0.0;
            }
            // The pressure trend, for every source that reports a
            // barometer (it used to be a Tempest-only derivation): the
            // last six hours of samples.
            if let Some(p) = wrote_pressure {
                let six_hours_ago = at_epoch - 6 * 3600;
                while roll
                    .pressure
                    .front()
                    .is_some_and(|(t, _)| *t < six_hours_ago)
                {
                    roll.pressure.pop_front();
                }
                roll.pressure.push_back((at_epoch, p));
                snap.pressure_trend_inhg = roll.pressure.iter().cloned().collect();
            }
            // A live reading also ages the strike ring, so the hourly
            // count decays to zero once a storm has passed rather than
            // freezing at its last total until the next strike.
            if live_current {
                let one_hour_ago = at_epoch - 3600;
                while roll
                    .strikes
                    .front()
                    .is_some_and(|s| s.time_epoch < one_hour_ago)
                {
                    roll.strikes.pop_front();
                }
                snap.lightning_strikes_last_hour = roll.strikes.len() as u32;
                if roll.strikes.len() != snap.lightning_recent.len() {
                    snap.lightning_recent = roll.strikes.iter().cloned().collect();
                }
                let last = roll.strikes.back();
                snap.last_strike_distance_mi = last.map(|s| crate::units::km_to_mi(s.distance_km));
                snap.last_strike_epoch = last.map(|s| s.time_epoch);
            }
        }
        // Record display provenance (separate lock, never nested with owners).
        if !source_label.is_empty() {
            let mut prov = self.field_provenance.lock().unwrap();
            for key in prov_keys {
                prov.insert(key, source_label.to_string());
            }
        }
        // Recompute the derived readings from the (possibly updated)
        // merged temp/rh/wind, whoever owns each.
        snap.feels_like_f = feels_like_f(snap.air_temp_f, snap.rh_pct, snap.wind_avg_mph);
        if owns_air_temp || owns_rh {
            snap.wet_bulb_f = crate::units::c_to_f(wet_bulb_c(
                crate::units::f_to_c(snap.air_temp_f),
                snap.rh_pct,
            ));
        }
        // Headline current-conditions provenance follows whoever owns the air
        // temperature; a source that only contributed (e.g.) pressure does NOT
        // hijack the headline label. A cloud that actually takes temperature
        // ownership must name itself too; retaining the station's label would
        // misattribute the displayed value after a pin or fallback.
        if owns_air_temp && !source_label.is_empty() {
            snap.source_label = source_label.to_string();
            snap.owner_priority = prio;
        }
        // Any LIVE write keeps the station-fresh signal current for the engine;
        // a forecast source must NOT (the engine would treat it as a live read).
        // Per-field live epochs let the engine tell a real live reading from a
        // forecast-filled one even while a partial live source keeps the
        // whole-snapshot timestamp fresh.
        if live_current {
            snap.last_packet_epoch = at_epoch;
            // A live source claimed at least one field this call (touched is set,
            // and live_current gates it): mark the station present. Only ever set
            // true here; the forecast-fill path below leaves prev's value intact,
            // so an Open-Meteo-only deployment stays cloud-only (false).
            if live_claimed {
                snap.has_live_station = true;
            }
        }
        // The epoch belongs to the value, not the previous owner. A cloud
        // takeover must clear the displaced station's timestamp even when
        // that timestamp is recent (for example an explicit cloud pin).
        let live_epoch = if live_current { at_epoch } else { 0 };
        if owns_air_temp {
            snap.air_temp_live_epoch = live_epoch;
        }
        if owns_wind {
            snap.wind_live_epoch = live_epoch;
        }
        if owns_rh {
            snap.rh_live_epoch = live_epoch;
        }
        if owns_rain {
            snap.rain_live_epoch = live_epoch;
        }
        let new = Arc::new(snap);
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    /// Batch strike insert: one lock, one snapshot swap, one SSE event
    /// for the whole slice. The station's detector reports one strike per
    /// event; the Blitzortung feed batches because the community network
    /// can deliver several strikes per second during an outbreak and each
    /// swap broadcasts a full snapshot.
    pub fn apply_strikes(&self, evts: &[StrikeEvent]) {
        let Some(newest) = evts.iter().max_by_key(|e| e.time_epoch).cloned() else {
            return;
        };
        let strikes: Vec<StrikeEvent> = {
            let mut roll = self.rolling.lock().unwrap();
            for evt in evts {
                // Located strikes (Blitzortung) carry a stable nanosecond
                // id. The community feed re-solves and RE-PUBLISHES a
                // strike under that same id as late station reports arrive,
                // often at a moved position; collapse those refinements to
                // one strike with last-write-wins position instead of
                // double-counting and double-plotting them. id == 0
                // (Tempest distance rings, legacy payloads) is never
                // deduped: each is a distinct event.
                if evt.id != 0 {
                    if let Some(existing) = roll.strikes.iter_mut().find(|s| s.id == evt.id) {
                        existing.lat = evt.lat;
                        existing.lon = evt.lon;
                        existing.distance_km = evt.distance_km;
                        continue;
                    }
                }
                roll.strikes.push_back(evt.clone());
            }
            // Trim to last hour.
            let one_hour_ago = newest.time_epoch - 3600;
            while roll
                .strikes
                .front()
                .is_some_and(|s| s.time_epoch < one_hour_ago)
            {
                roll.strikes.pop_front();
            }
            // Hard cap. The local Tempest alone never gets near it
            // (strikes are rare per-minute events), but the Blitzortung
            // community feed can keep hundreds in the hour window, and
            // the buffer is serialized into every snapshot/SSE payload,
            // so it must stay bounded regardless of storm size.
            while roll.strikes.len() > MAX_RECENT_STRIKES {
                roll.strikes.pop_front();
            }
            roll.strikes.iter().cloned().collect()
        };
        let prev = self.current.load_full();
        let count = strikes.len() as u32;
        let new = Arc::new(Snapshot {
            lightning_strikes_last_hour: count,
            lightning_recent: strikes,
            last_strike_distance_mi: Some(newest.distance_km * 0.621371),
            last_strike_epoch: Some(newest.time_epoch),
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    /// Which hardware is talking: the serials a LAN station reports.
    /// Published by the source on its first packet and on change; the
    /// footer shows them and `station_present` reads off them.
    pub fn apply_identity(&self, station_serial: &str, hub_serial: &str) {
        let prev = self.current.load_full();
        if prev.station_serial == station_serial && prev.hub_serial == hub_serial {
            return;
        }
        let new = Arc::new(Snapshot {
            station_serial: station_serial.to_string(),
            hub_serial: hub_serial.to_string(),
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }
}

#[cfg(all(test, feature = "ssr"))]
mod report_age_tests {
    use super::*;
    use crate::ports::weather_source::WeatherField as F;
    const NOW: i64 = 1_700_000_000;

    fn store() -> LiveWeatherStore {
        let store = LiveWeatherStore::new();
        store.set_priorities(HashMap::from([
            ("primary".into(), 100),
            ("backup".into(), 50),
        ]));
        store.set_max_ages(HashMap::from([
            ("primary".into(), 60),
            ("backup".into(), 1200),
        ]));
        store
    }

    #[test]
    fn expired_primary_cannot_reclaim_a_fresh_fallback_on_repeated_polls() {
        let store = store();
        store.apply_received_fields(&[(F::AirTempF, 68.0)], NOW, NOW, true, "backup");
        for received in [NOW, NOW + 30] {
            store.apply_received_fields(
                &[(F::AirTempF, 77.0)],
                NOW - 120,
                received,
                true,
                "primary",
            );
            assert_eq!(store.snapshot().air_temp_f, 68.0);
            assert_eq!(store.snapshot().air_temp_live_epoch, NOW);
        }
        assert_eq!(
            store.current_condition_samples(NOW)[0]
                .as_ref()
                .unwrap()
                .max_age_s,
            1200
        );
        store.apply_received_fields(&[(F::AirTempF, 77.0)], NOW + 30, NOW + 30, true, "primary");
        assert_eq!(store.snapshot().air_temp_f, 77.0);
        assert_eq!(
            store.current_condition_samples(NOW)[0]
                .as_ref()
                .unwrap()
                .max_age_s,
            60
        );
    }

    #[test]
    fn older_fresh_backup_does_not_make_a_newer_primary_look_future_dated() {
        let store = store();
        store.apply_received_fields(&[(F::AirTempF, 77.0)], NOW, NOW, true, "primary");
        store.apply_received_fields(&[(F::AirTempF, 68.0)], NOW - 30, NOW, true, "backup");
        assert_eq!(store.snapshot().air_temp_f, 77.0);
        // The same backup report remains valid when the primary really expires.
        store.apply_received_fields(&[(F::AirTempF, 68.0)], NOW - 30, NOW + 61, true, "backup");
        assert_eq!(store.snapshot().air_temp_f, 68.0);
        assert_eq!(store.snapshot().air_temp_live_epoch, NOW - 30);
    }

    #[test]
    fn cloud_fallback_uses_arrival_clock_without_becoming_a_live_station() {
        let store = store();
        store.apply_received_fields(&[(F::AirTempF, 77.0)], NOW, NOW, true, "primary");
        store.apply_received_fields(&[(F::AirTempF, 68.0)], NOW - 30, NOW + 61, false, "backup");
        assert_eq!(store.snapshot().air_temp_f, 68.0);
        assert_eq!(store.snapshot().air_temp_live_epoch, 0);
    }

    #[test]
    fn source_pin_and_ordered_chain_use_report_age_at_arrival() {
        for chained in [false, true] {
            let store = store();
            if chained {
                store.set_field_chains(HashMap::from([(
                    "air_temp_f",
                    vec!["primary".into(), "backup".into()],
                )]));
            } else {
                store.set_field_overrides(HashMap::from([("air_temp_f", "primary".into())]));
            }
            store.apply_received_fields(&[(F::AirTempF, 77.0)], NOW, NOW, true, "primary");
            store.apply_received_fields(&[(F::AirTempF, 68.0)], NOW - 30, NOW, true, "backup");
            assert_eq!(store.snapshot().air_temp_f, 77.0);
            store.apply_received_fields(&[(F::AirTempF, 68.0)], NOW - 30, NOW + 61, true, "backup");
            store.apply_received_fields(&[(F::AirTempF, 77.0)], NOW, NOW + 61, true, "primary");
            assert_eq!(store.snapshot().air_temp_f, 68.0);
        }
    }

    #[test]
    fn pinned_cloud_clears_each_displaced_live_epoch_and_names_its_temperature() {
        let store = store();
        let fields = [
            (F::AirTempF, 77.0),
            (F::WindMph, 4.0),
            (F::RhPct, 50.0),
            (F::RainIntensityInHr, 0.0),
        ];
        store.apply_received_fields(&fields, NOW, NOW, true, "primary");
        store.set_field_overrides(
            [
                "air_temp_f",
                "wind_avg_mph",
                "rh_pct",
                "rain_intensity_in_hr",
            ]
            .map(|key| (key, "backup".into()))
            .into(),
        );
        store.apply_received_fields(&fields, NOW, NOW, false, "backup");
        let snap = store.snapshot();
        assert_eq!(
            [
                snap.air_temp_live_epoch,
                snap.wind_live_epoch,
                snap.rh_live_epoch,
                snap.rain_live_epoch
            ],
            [0; 4]
        );
        assert_eq!(snap.source_label, "backup");
        assert_eq!(
            crate::assembly::readings::resolve_current_conditions(
                &store.current_condition_samples(NOW),
                None,
                NOW,
            )
            .3,
            crate::engine::skip_rules::LiveReadings::ForecastFallback
        );
    }
}

#[cfg(all(test, feature = "ssr"))]
mod rain_day_tests {
    use super::*;
    use chrono::TimeZone;

    /// UNIX epoch for a LOCAL wall-clock instant. Test times stay clear
    /// of the 02:00-03:00 DST-transition window so `.single()` is safe.
    fn local_epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    /// One reported minute from a station, the way the Tempest source
    /// publishes it: the minute's fall, not a since-midnight total.
    fn minute(store: &LiveWeatherStore, epoch: i64, rain_mm: f64) {
        store.apply_source_fields(
            &[(
                crate::ports::weather_source::WeatherField::RainLastMinIn,
                crate::units::mm_to_in(rain_mm),
            )],
            epoch,
            true,
            "tempest",
        );
    }

    #[test]
    fn local_day_ordinal_buckets_by_local_date() {
        let d1a = local_epoch(2026, 3, 3, 0, 30);
        let d1b = local_epoch(2026, 3, 3, 23, 30);
        let d2 = local_epoch(2026, 3, 4, 0, 30);
        assert_eq!(local_day_ordinal(d1a), local_day_ordinal(d1b));
        assert_eq!(local_day_ordinal(d2), local_day_ordinal(d1b) + 1);
    }

    #[test]
    fn rain_today_accumulates_within_a_local_day_and_resets_at_local_midnight() {
        let store = LiveWeatherStore::new();
        // 23:30 local: 5.08 mm = 0.20".
        minute(&store, local_epoch(2026, 1, 15, 23, 30), 5.08);
        assert!((store.snapshot().rain_in_today - 0.20).abs() < 1e-9);
        // 23:50 same local day: accumulates to 0.30".
        minute(&store, local_epoch(2026, 1, 15, 23, 50), 2.54);
        assert!((store.snapshot().rain_in_today - 0.30).abs() < 1e-9);
        // 00:30 the NEXT local day: bucket rolls, total restarts at 0.10".
        // With the old UTC bucketing this either kept accumulating or had
        // already reset hours before local midnight, tz-dependent.
        minute(&store, local_epoch(2026, 1, 16, 0, 30), 2.54);
        assert!((store.snapshot().rain_in_today - 0.10).abs() < 1e-9);
    }

    #[test]
    fn duplicate_and_late_minutes_cannot_inflate_or_rewind_measured_rain() {
        let store = LiveWeatherStore::new();
        let late = local_epoch(2026, 1, 15, 23, 59);
        minute(&store, late, 5.08);
        minute(&store, late, 5.08);
        minute(&store, late - 60, 5.08);
        assert!((store.snapshot().rain_in_today - 0.20).abs() < 1e-9);
        assert_eq!(store.snapshot().last_packet_epoch, late);
        let next_day = local_epoch(2026, 1, 16, 0, 1);
        minute(&store, next_day, 2.54);
        minute(&store, late, 5.08);
        assert!((store.snapshot().rain_in_today - 0.10).abs() < 1e-9);
        assert_eq!(store.snapshot().last_packet_epoch, next_day);
        assert_eq!(
            store.snapshot().rain_today_day_ordinal,
            local_day_ordinal(next_day)
        );
    }

    #[test]
    fn rain_increment_sources_keep_independent_priority_resolved_totals() {
        use crate::ports::weather_source::WeatherField as F;
        let store = LiveWeatherStore::new();
        store.set_priorities(HashMap::from([
            ("primary".into(), 100),
            ("backup".into(), 50),
        ]));
        let at = local_epoch(2026, 1, 15, 12, 0);
        let publish = |source, epoch, rain| {
            store.apply_source_fields(&[(F::RainLastMinIn, rain)], epoch, true, source);
        };
        publish("primary", at, 0.10);
        publish("backup", at + 1, 0.70);
        publish("primary", at + 60, 0.20);
        assert!((store.snapshot().rain_in_today - 0.30).abs() < 1e-9);
        assert!((store.snapshot().rain_in_last_min - 0.20).abs() < 1e-9);
        // The primary has gone stale. Failover uses the backup's own
        // complete total, including the minute it offered while outranked.
        publish("backup", at + 700, 0.05);
        assert!((store.snapshot().rain_in_today - 0.75).abs() < 1e-9);
        assert_eq!(store.rain_today_owner(at + 700).unwrap().label, "backup");
        publish("primary", at + 710, 0.10);
        assert!((store.snapshot().rain_in_today - 0.40).abs() < 1e-9);
    }

    #[test]
    fn invalid_minute_does_not_poison_the_accumulator_or_claim_freshness() {
        let store = LiveWeatherStore::new();
        let at = local_epoch(2026, 1, 15, 12, 0);
        minute(&store, at, 2.54);
        minute(&store, at + 60, f64::NAN);
        minute(&store, at + 60, f64::INFINITY);
        minute(&store, at + 60, -1.0);
        assert_eq!(store.snapshot().last_packet_epoch, at);
        minute(&store, at + 60, 2.54);
        assert!((store.snapshot().rain_in_today - 0.20).abs() < 1e-9);
    }

    #[test]
    fn et0_today_resets_at_local_midnight() {
        use crate::ports::weather_source::WeatherField as F;
        let store = LiveWeatherStore::new();
        // A live station reports the since-midnight accumulator late in the day.
        let t1 = local_epoch(2026, 1, 15, 23, 30);
        store.apply_source_fields(&[(F::Et0Today, 4.2)], t1, true, "davis");
        assert!((store.snapshot().et0_today - 4.2).abs() < 1e-9);
        assert!(store.et0_today_is_live(t1), "fresh live owner");
        // An obs_st on the NEXT local day must not carry yesterday's total
        // (the pre-fix carry-forward pinned it until the source spoke again).
        minute(&store, local_epoch(2026, 1, 16, 0, 30), 0.0);
        assert!((store.snapshot().et0_today - 0.0).abs() < 1e-9);
    }

    #[test]
    fn et0_today_zeroes_on_next_day_bus_write_and_carries_within_the_day() {
        use crate::ports::weather_source::WeatherField as F;
        let store = LiveWeatherStore::new();
        let t1 = local_epoch(2026, 1, 15, 22, 0);
        store.apply_source_fields(&[(F::Et0Today, 3.8)], t1, true, "davis");
        // A same-day write of an unrelated field carries the accumulator.
        store.apply_source_fields(
            &[(F::AirTempF, 50.0)],
            local_epoch(2026, 1, 15, 23, 0),
            true,
            "davis",
        );
        assert!((store.snapshot().et0_today - 3.8).abs() < 1e-9);
        // The same unrelated write on the NEXT local day zeroes it: the day
        // rolled and no source has reported today's ET0 yet.
        store.apply_source_fields(
            &[(F::AirTempF, 48.0)],
            local_epoch(2026, 1, 16, 1, 0),
            true,
            "davis",
        );
        assert!((store.snapshot().et0_today - 0.0).abs() < 1e-9);
    }

    #[test]
    fn et0_cloud_fill_is_not_live() {
        use crate::ports::weather_source::WeatherField as F;
        let store = LiveWeatherStore::new();
        let t = local_epoch(2026, 1, 15, 12, 0);
        // A forecast fill (live_current=false) writes the full-day figure but
        // must never read as a live station accumulator.
        store.apply_source_fields(&[(F::Et0Today, 4.6)], t, false, "open_meteo");
        assert!((store.snapshot().et0_today - 4.6).abs() < 1e-9);
        assert!(!store.et0_today_is_live(t), "cloud fill is not live");
    }

    #[test]
    fn restored_minutes_keep_their_identity_source_and_local_day() {
        let store = LiveWeatherStore::new();
        let now = chrono::Utc::now().timestamp();
        assert!(!store.restore_rain_minutes("ha", 0.5, now - 2 * 86_400, now));
        assert!(!store.restore_rain_minutes("ha", 0.5, now + 1, now));
        assert!(store.restore_rain_minutes("ha", 0.5, now, now));
        assert!(!store.restore_rain_minutes("ha", 0.1, now, now));
        assert!((store.accumulate_rain_mm("ha", 2.54, now).unwrap() - 12.7).abs() < 1e-6);
        assert!(store.accumulate_rain_mm("ha", 2.54, now - 1).is_none());
        assert!((store.accumulate_rain_mm("ha", 2.54, now + 1).unwrap() - 15.24).abs() < 1e-6);
        assert!((store.accumulate_rain_mm("other", 2.54, now).unwrap() - 2.54).abs() < 1e-6);
        assert!(store.restore_rain_minutes("dry", 0.0, now, now));
        assert_eq!(store.accumulate_rain_mm("dry", 0.0, now), Some(0.0));
    }
}

#[cfg(all(test, feature = "ssr"))]
mod strike_buffer_tests {
    use super::*;

    fn strike(epoch: i64, dist_km: f64) -> StrikeEvent {
        StrikeEvent {
            time_epoch: epoch,
            distance_km: dist_km,
            ..Default::default()
        }
    }

    #[test]
    fn ring_prunes_strikes_older_than_one_hour() {
        let store = LiveWeatherStore::new();
        let t0 = 1_700_000_000;
        store.apply_strikes(&[strike(t0, 5.0)]);
        store.apply_strikes(&[strike(t0 + 60, 8.0)]);
        assert_eq!(store.snapshot().lightning_strikes_last_hour, 2);
        // A strike >1h later evicts both earlier ones.
        store.apply_strikes(&[strike(t0 + 3700, 12.0)]);
        let snap = store.snapshot();
        assert_eq!(snap.lightning_strikes_last_hour, 1);
        assert_eq!(snap.lightning_recent.len(), 1);
        assert_eq!(snap.last_strike_epoch, Some(t0 + 3700));
    }

    /// A live reading with no lightning in it. Any live observation ages
    /// the strike ring, which is what makes the published count decay once
    /// a storm has passed.
    fn quiet_minute(store: &LiveWeatherStore, epoch: i64) {
        store.apply_source_fields(
            &[
                (crate::ports::weather_source::WeatherField::AirTempF, 70.0),
                (
                    crate::ports::weather_source::WeatherField::LightningCount,
                    0.0,
                ),
            ],
            epoch,
            true,
            "tempest",
        );
    }

    #[test]
    fn counter_decays_to_zero_once_strikes_age_out() {
        // The storm-ended case. Every live reading trims the buffer, so
        // the published counter must follow it down; carrying the previous
        // count forward froze it at the storm's total until the next strike,
        // which left a "strikes above 0" alert armed and silent for weeks.
        let store = LiveWeatherStore::new();
        let t0 = 1_700_000_000;
        store.apply_strikes(&[strike(t0, 5.0)]);
        store.apply_strikes(&[strike(t0 + 60, 8.0)]);
        assert_eq!(store.snapshot().lightning_strikes_last_hour, 2);

        // A quiet observation while the strikes are still inside the hour
        // leaves the count alone.
        quiet_minute(&store, t0 + 1800);
        assert_eq!(store.snapshot().lightning_strikes_last_hour, 2);

        // An observation past the last strike's hour drains the buffer, and
        // the counter goes with it (no further strikes needed).
        quiet_minute(&store, t0 + 3700);
        let snap = store.snapshot();
        assert_eq!(snap.lightning_strikes_last_hour, 0, "counter must decay");
        assert_eq!(snap.last_strike_distance_mi, None);
        assert_eq!(snap.last_strike_epoch, None);
    }

    #[test]
    fn avg_distance_is_none_in_an_interval_with_no_strikes() {
        // The packet reports a bare 0 for a quiet minute. On a distance
        // channel that reads as "overhead", so it must publish as no reading.
        let store = LiveWeatherStore::new();
        let t0 = 1_700_000_000;
        quiet_minute(&store, t0);
        assert_eq!(store.snapshot().lightning_avg_dist_mi, None);

        // A minute WITH strikes carries the real average (10 km = 6.21 mi).
        store.apply_source_fields(
            &[
                (
                    crate::ports::weather_source::WeatherField::LightningCount,
                    3.0,
                ),
                (
                    crate::ports::weather_source::WeatherField::LightningDistanceMi,
                    crate::units::km_to_mi(10.0),
                ),
            ],
            t0 + 60,
            true,
            "tempest",
        );
        let mi = store
            .snapshot()
            .lightning_avg_dist_mi
            .expect("real average");
        assert!((mi - 6.21371).abs() < 1e-4, "got {mi}");

        // The next quiet minute reverts to no reading rather than 0 miles.
        quiet_minute(&store, t0 + 120);
        assert_eq!(store.snapshot().lightning_avg_dist_mi, None);
    }

    #[test]
    fn ring_caps_at_500_strikes() {
        // A Blitzortung-scale burst (600 strikes inside the hour) must
        // not grow the snapshot beyond the cap; the oldest fall off.
        let store = LiveWeatherStore::new();
        let t0 = 1_700_000_000;
        let batch: Vec<StrikeEvent> = (0..600).map(|i| strike(t0 + i, 10.0)).collect();
        store.apply_strikes(&batch);
        let snap = store.snapshot();
        assert_eq!(snap.lightning_recent.len(), 500);
        assert_eq!(snap.lightning_strikes_last_hour, 500);
        // Oldest 100 evicted, newest kept.
        assert_eq!(snap.lightning_recent[0].time_epoch, t0 + 100);
        assert_eq!(snap.last_strike_epoch, Some(t0 + 599));
    }

    #[test]
    fn batch_apply_swaps_snapshot_once_and_mixes_sources() {
        let store = LiveWeatherStore::new();
        let mut rx = store.subscribe();
        rx.mark_unchanged();
        let community = StrikeEvent {
            time_epoch: 1_700_000_100,
            distance_km: 42.0,
            source: crate::tempest::packets::STRIKE_SOURCE_BLITZORTUNG.to_string(),
            lat: Some(28.5),
            lon: Some(-81.4),
            ..Default::default()
        };
        store.apply_strikes(&[strike(1_700_000_000, 7.0), community]);
        // Exactly one watch notification for the batch.
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        assert!(!rx.has_changed().unwrap());
        let snap = store.snapshot();
        assert_eq!(snap.lightning_recent.len(), 2);
        assert_eq!(snap.lightning_recent[0].source, "tempest");
        assert_eq!(snap.lightning_recent[1].source, "blitzortung");
        assert_eq!(snap.lightning_recent[1].lat, Some(28.5));
        // last_strike_* follows the newest of the batch.
        assert_eq!(snap.last_strike_epoch, Some(1_700_000_100));
    }

    fn located(id: i64, epoch: i64, lat: f64, lon: f64, dist_km: f64) -> StrikeEvent {
        StrikeEvent {
            time_epoch: epoch,
            distance_km: dist_km,
            source: crate::tempest::packets::STRIKE_SOURCE_BLITZORTUNG.to_string(),
            lat: Some(lat),
            lon: Some(lon),
            id,
            ..Default::default()
        }
    }

    #[test]
    fn refinements_dedup_by_id_with_last_write_wins_position() {
        // Blitzortung re-publishes a strike under the same nanosecond id
        // as it re-solves, sometimes at a moved position. The buffer must
        // keep ONE entry (no double count) at the LATEST position.
        let store = LiveWeatherStore::new();
        let t = 1_700_000_000;
        store.apply_strikes(&[located(111, t, 28.5, -81.4, 40.0)]);
        // Refinement: same id, moved a few km, arrives in a later batch.
        store.apply_strikes(&[located(111, t, 28.55, -81.35, 41.0)]);
        let snap = store.snapshot();
        assert_eq!(
            snap.lightning_recent.len(),
            1,
            "refinement must not add a dot"
        );
        assert_eq!(
            snap.lightning_strikes_last_hour, 1,
            "refinement must not inflate count"
        );
        assert_eq!(
            snap.lightning_recent[0].lat,
            Some(28.55),
            "last-write-wins position"
        );
        assert_eq!(snap.lightning_recent[0].lon, Some(-81.35));

        // A distinct strike (different id) is a separate dot.
        store.apply_strikes(&[located(222, t + 1, 28.6, -81.3, 42.0)]);
        assert_eq!(store.snapshot().lightning_recent.len(), 2);

        // id == 0 (Tempest distance rings) is never deduped even at the
        // same epoch: two rings stay two.
        store.apply_strikes(&[strike(t + 2, 5.0)]);
        store.apply_strikes(&[strike(t + 2, 6.0)]);
        assert_eq!(store.snapshot().lightning_recent.len(), 4);
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let store = LiveWeatherStore::new();
        let mut rx = store.subscribe();
        rx.mark_unchanged();
        store.apply_strikes(&[]);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(store.snapshot().last_strike_epoch, None);
    }
}

#[cfg(all(test, feature = "ssr"))]
mod bridge_tests {
    use super::*;
    use crate::ports::weather_source::WeatherField as F;
    use crate::weather::arbitration::MRMS_WRITER_LABEL;

    /// A per-minute entity mapped to the daily rain field is caught, and
    /// the source is named so the UI can point at it.
    ///
    /// This is the single most damaging misconfiguration available: the
    /// value is classified as measured gauge data and written into the
    /// day-max observations ledger, where it outranks the model rain
    /// archive for the whole balance window.
    #[test]
    fn rain_that_falls_within_a_day_is_rejected_and_the_source_named() {
        let store = LiveWeatherStore::new();
        // 2026-09-05 12:00 UTC and a minute later.
        let noon = 1_788_609_600;
        store.apply_source_fields(&[(F::RainTodayIn, 0.42)], noon, true, "ha_bridge");
        assert_eq!(store.snapshot().rain_in_today, 0.42);
        assert_eq!(store.snapshot().rain_today_suspect_source, None);

        // The next minute is dry, so a per-minute entity reports ~0.
        store.apply_source_fields(&[(F::RainTodayIn, 0.0)], noon + 60, true, "ha_bridge");
        assert_eq!(
            store.snapshot().rain_in_today,
            0.42,
            "the day's total must not fall"
        );
        assert_eq!(
            store.snapshot().rain_today_suspect_source.as_deref(),
            Some("ha_bridge"),
            "and the source is named so the UI can say which mapping is wrong"
        );
    }

    /// A REAL accumulator resets to zero at local midnight, and that must
    /// go through. A guard that blocked it would pin the total at
    /// yesterday's value forever, which is worse than the bug it fixes.
    #[test]
    fn a_real_accumulator_may_reset_at_the_start_of_a_new_day() {
        let store = LiveWeatherStore::new();
        let noon = 1_788_609_600; // 2026-09-05 12:00 UTC
        store.apply_source_fields(&[(F::RainTodayIn, 0.42)], noon, true, "ha_bridge");
        assert_eq!(store.snapshot().rain_in_today, 0.42);

        // The next local day: the accumulator legitimately starts over.
        store.apply_source_fields(&[(F::RainTodayIn, 0.0)], noon + 86_400, true, "ha_bridge");
        assert_eq!(
            store.snapshot().rain_in_today,
            0.0,
            "a new day resets, and the guard must not stand in the way"
        );
        assert_eq!(
            store.snapshot().rain_today_suspect_source,
            None,
            "and the suspicion clears with the new day"
        );
    }

    /// The guard only watches a source against ITSELF. A fall across an
    /// ownership change is ordinary failover: when a primary goes stale
    /// the next source takes over with its own, possibly lower, number.
    #[test]
    fn a_lower_total_from_a_different_source_is_not_suspicious() {
        let store = LiveWeatherStore::new();
        let mut prio = HashMap::new();
        prio.insert("primary".to_string(), 80);
        prio.insert("backup".to_string(), 70);
        store.set_priorities(prio);
        let mut ages = HashMap::new();
        ages.insert("primary".to_string(), 600);
        ages.insert("backup".to_string(), 600);
        store.set_max_ages(ages);

        let noon = 1_788_609_600;
        store.apply_source_fields(&[(F::RainTodayIn, 0.42)], noon, true, "primary");
        // The primary goes quiet past its window, so the backup takes the
        // field with its own, lower, total. Same local day.
        store.apply_source_fields(&[(F::RainTodayIn, 0.10)], noon + 700, true, "backup");
        assert_eq!(
            store.snapshot().rain_in_today,
            0.10,
            "a different source owning the field is failover, not a bad mapping"
        );
        assert_eq!(store.snapshot().rain_today_suspect_source, None);
    }

    #[test]
    fn forecast_source_populates_display_but_not_liveness() {
        // The whole containment for Issue 2: a forecast source (live_current=
        // false) fills the dashboard display fields but must NOT stamp
        // last_packet_epoch, or resolve_current_conditions mislabels it as a
        // live station and feeds forecast numbers into a run/skip decision.
        let store = LiveWeatherStore::new();
        store.apply_source_fields(
            &[
                (F::AirTempF, 71.6),
                (F::RhPct, 55.0),
                (F::PressureInHg, 29.9),
            ],
            1_000,
            false,
            "forecast",
        );
        let s = store.snapshot();
        assert_eq!(s.air_temp_f, 71.6);
        assert_eq!(s.rh_pct, 55.0);
        assert_eq!(s.pressure_inhg, 29.9);
        assert_eq!(
            s.last_packet_epoch, 0,
            "a forecast source must never claim station-liveness"
        );
    }

    #[test]
    fn live_station_stamps_liveness() {
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 2_000, true, "test");
        let s = store.snapshot();
        assert_eq!(s.air_temp_f, 70.0);
        assert_eq!(s.last_packet_epoch, 2_000);
    }

    #[test]
    fn carries_unset_fields_forward() {
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::AirTempF, 60.0)], 1_000, true, "test");
        store.apply_source_fields(&[(F::RhPct, 40.0)], 1_001, true, "test");
        let s = store.snapshot();
        assert_eq!(s.air_temp_f, 60.0, "temp survives a humidity-only update");
        assert_eq!(s.rh_pct, 40.0);
    }

    #[test]
    fn wind_maps_to_avg_and_structured_forecast_ignored() {
        let store = LiveWeatherStore::new();
        store.apply_source_fields(
            &[(F::WindMph, 12.0), (F::ForecastDaily, 0.0)],
            1_000,
            true,
            "test",
        );
        let s = store.snapshot();
        assert_eq!(s.wind_avg_mph, 12.0, "WindMph maps to wind_avg_mph");
        assert_eq!(s.last_packet_epoch, 1_000);
    }

    #[test]
    fn et0_flow_pop_populate_the_snapshot() {
        // A1: these used to be silently dropped; now they reach the snapshot
        // (so HA/dashboard see flow + ET0 + POP, and the engine can read ET0).
        let store = LiveWeatherStore::new();
        store.apply_source_fields(
            &[
                (F::Et0Today, 4.2),
                (F::FlowGpm, 12.0),
                (F::FlowTotalGalToday, 340.0),
                (F::Pop, 65.0),
            ],
            1_000,
            true,
            "test",
        );
        let s = store.snapshot();
        assert_eq!(s.et0_today, 4.2);
        assert_eq!(s.flow_gpm, 12.0);
        assert_eq!(s.flow_total_gal_today, 340.0);
        assert_eq!(s.pop_pct, Some(65.0));
    }

    #[test]
    fn pop_and_leaf_stay_null_until_a_source_writes_them() {
        // Unknown means unknown: a snapshot no source has fed Pop or leaf
        // wetness serializes both null, never a fabricated 0% ("certainly no
        // rain" / "bone-dry canopy"). Same day-bucket-era contract as
        // lightning_avg_dist_mi.
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 1_000, true, "test");
        let s = store.snapshot();
        assert_eq!(s.pop_pct, None);
        assert_eq!(s.leaf_wetness_pct, None);

        store.apply_source_fields(&[(F::LeafWetness, 0.0)], 1_100, true, "test");
        let s = store.snapshot();
        assert_eq!(
            s.leaf_wetness_pct,
            Some(0.0),
            "a REPORTED 0% is a real dry-canopy reading, distinct from unknown"
        );
    }

    #[test]
    fn structured_forecast_only_batch_is_a_noop() {
        // ForecastDaily/Hourly are structured (carried by SourceEvent::Forecast,
        // not this scalar path), so a batch of only those touches nothing.
        let store = LiveWeatherStore::new();
        store.apply_source_fields(
            &[(F::ForecastDaily, 0.0), (F::ForecastHourly, 0.0)],
            9_000,
            true,
            "test",
        );
        assert_eq!(store.snapshot().last_packet_epoch, 0);
    }

    #[test]
    fn forecast_does_not_clobber_a_fresh_live_station() {
        // The common Tempest + forecast config: a live station owns the
        // snapshot; a forecast source arriving moments later must NOT overwrite
        // the live reading (the engine reads it while last_packet_epoch is fresh).
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 1_000, true, "station"); // live station
        store.apply_source_fields(&[(F::AirTempF, 50.0)], 1_100, false, "forecast"); // forecast, fresh window
        assert_eq!(
            store.snapshot().air_temp_f,
            70.0,
            "forecast must not overwrite a fresh live station"
        );
    }

    #[test]
    fn source_label_records_provenance() {
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 1_000, true, "Ecowitt");
        assert_eq!(store.snapshot().source_label, "Ecowitt");
    }

    #[test]
    fn single_live_source_always_owns() {
        // The common case (and Tempest-only setups): one live source keeps
        // owning current conditions on every refresh, unaffected by arbitration.
        let store = LiveWeatherStore::new();
        for (i, t) in [70.0, 71.0, 72.0].into_iter().enumerate() {
            store.apply_source_fields(&[(F::AirTempF, t)], 1_000 + i as i64, true, "ecowitt");
            assert_eq!(store.snapshot().air_temp_f, t);
        }
    }

    #[test]
    fn higher_priority_live_source_owns_current() {
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("ecowitt".to_string(), 70);
        p.insert("davis".to_string(), 60);
        store.set_priorities(p);
        // Ecowitt (70) claims first.
        store.apply_source_fields(&[(F::AirTempF, 60.0)], 1_000, true, "ecowitt");
        // Davis (60) is lower and the owner is fresh -> suppressed.
        store.apply_source_fields(&[(F::AirTempF, 99.0)], 1_010, true, "davis");
        assert_eq!(
            store.snapshot().air_temp_f,
            60.0,
            "lower-priority live source must not seize a fresh owner"
        );
        // Ecowitt refresh still wins.
        store.apply_source_fields(&[(F::AirTempF, 61.0)], 1_020, true, "ecowitt");
        assert_eq!(store.snapshot().air_temp_f, 61.0);
    }

    #[test]
    fn partial_source_does_not_zero_other_fields() {
        // The real-world bug: a soil gateway with only a barometer must NOT zero
        // out the temp/RH/wind a full station provides, even at EQUAL priority.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100);
        p.insert("ecowitt_gw".to_string(), 100);
        store.set_priorities(p);
        store.apply_source_fields(
            &[(F::AirTempF, 72.0), (F::RhPct, 55.0), (F::WindMph, 8.0)],
            1_000,
            true,
            "tempest",
        );
        // Barometer-only gateway at equal priority adds pressure.
        store.apply_source_fields(&[(F::PressureInHg, 29.98)], 1_010, true, "ecowitt_gw");
        let s = store.snapshot();
        assert_eq!(s.air_temp_f, 72.0, "partial source must not zero temp");
        assert_eq!(s.rh_pct, 55.0);
        assert_eq!(s.wind_avg_mph, 8.0);
        assert_eq!(s.pressure_inhg, 29.98, "pressure comes from the gateway");
        assert_eq!(
            s.source_label, "tempest",
            "headline = air-temp owner, not the barometer"
        );
    }

    #[test]
    fn a_station_keeps_its_weather_when_a_partial_gateway_adds_pressure() {
        // A station reporting everything and an Ecowitt gateway reporting
        // only a barometer, at equal priority: the gateway must own
        // pressure and nothing else.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100);
        p.insert("ecowitt_gw".to_string(), 100);
        store.set_priorities(p);
        store.apply_source_fields(
            &[
                (F::AirTempF, 71.96),
                (F::RhPct, 55.0),
                (F::WindMph, crate::units::ms_to_mph(3.0)),
                (F::PressureInHg, crate::units::hpa_to_inhg(1013.0)),
            ],
            1_000,
            true,
            "tempest",
        );
        store.apply_source_fields(&[(F::PressureInHg, 30.10)], 1_010, true, "ecowitt_gw");
        let s = store.snapshot();
        assert!(
            (s.air_temp_f - 71.96).abs() < 0.1,
            "Tempest temp preserved: {}",
            s.air_temp_f
        );
        assert!(
            s.rh_pct > 0.0 && s.wind_avg_mph > 0.0,
            "Tempest rh/wind preserved"
        );
        assert_eq!(
            s.source_label, "tempest",
            "headline stays Tempest (air-temp owner)"
        );
    }

    #[test]
    fn conditions_provenance_reports_per_field_source() {
        // The UI panel: Tempest drives temp/wind/RH, a STRICTLY-higher-priority
        // barometer gateway drives pressure. Under strict `>` arbitration an
        // equal-priority gateway can no longer steal a field Tempest already
        // claimed (that was the thrash); a dedicated barometer that should win
        // pressure is given a higher pressure priority, the realistic config.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100);
        p.insert("ecowitt_gw".to_string(), 110); // strictly higher -> owns pressure
        store.set_priorities(p);
        store.apply_source_fields(
            &[
                (F::AirTempF, 71.6),
                (F::RhPct, 55.0),
                (F::WindMph, crate::units::ms_to_mph(3.0)),
                (F::PressureInHg, crate::units::hpa_to_inhg(1013.0)),
            ],
            1_000,
            true,
            "tempest",
        );
        store.apply_source_fields(&[(F::PressureInHg, 30.0)], 1_010, true, "ecowitt_gw");
        let prov: std::collections::HashMap<_, _> =
            store.conditions_provenance().into_iter().collect();
        assert_eq!(prov.get("Air temperature"), Some(&"tempest".to_string()));
        assert_eq!(prov.get("Wind"), Some(&"tempest".to_string()));
        assert_eq!(prov.get("Humidity"), Some(&"tempest".to_string()));
        assert_eq!(prov.get("Pressure"), Some(&"ecowitt_gw".to_string()));
    }

    #[test]
    fn priority_reload_reranks_silent_incumbents_in_live_and_cloud_tiers() {
        for live in [true, false] {
            let store = LiveWeatherStore::new();
            let rank = |primary, backup| {
                HashMap::from([
                    ("primary".to_string(), primary),
                    ("backup".to_string(), backup),
                ])
            };
            store.set_priorities(rank(100, 80));
            store.apply_source_fields(
                &[(F::AirTempF, 70.0), (F::WindMph, 3.0)],
                1_000,
                live,
                "primary",
            );

            // Only the incumbent's configured rank changes. It sends no new
            // sample, and its last value remains fresh throughout this test.
            store.set_priorities(rank(10, 80));
            store.apply_source_fields(&[(F::WindMph, 9.0)], 1_010, live, "backup");
            assert_eq!(
                store.snapshot().wind_avg_mph,
                9.0,
                "live={live}: silent owner's old rank must not block failover"
            );
            let provenance = store.field_source_map();
            assert_eq!(
                provenance.get("wind_mph").map(String::as_str),
                Some("backup")
            );
            assert_eq!(
                provenance.get("air_temp_f").map(String::as_str),
                Some("primary")
            );
            assert_eq!(
                store.snapshot().air_temp_f,
                70.0,
                "a partial replacement does not invent temperature"
            );

            // Promote the NEW incumbent without another sample. A contender
            // above its old rank but below its current one must stay secondary.
            store.set_priorities(rank(90, 100));
            store.apply_source_fields(&[(F::WindMph, 12.0)], 1_020, live, "primary");
            assert_eq!(
                store.snapshot().wind_avg_mph,
                9.0,
                "live={live}: promotion applies to the incumbent too"
            );

            // Equal ranks keep the fresh incumbent; its own refresh still wins.
            store.set_priorities(rank(90, 90));
            store.apply_source_fields(&[(F::WindMph, 12.0)], 1_030, live, "primary");
            assert_eq!(store.snapshot().wind_avg_mph, 9.0);
            store.apply_source_fields(&[(F::WindMph, 10.0)], 1_040, live, "backup");
            assert_eq!(store.snapshot().wind_avg_mph, 10.0);
        }
    }

    #[test]
    fn resolved_flow_preserves_evidence_priority_fallback_and_missing_totals() {
        let store = LiveWeatherStore::new();
        assert_eq!(
            store.flow_readout(1_000),
            crate::model::FlowReadout::default(),
            "raw zero defaults are not readings"
        );
        store.set_priorities(HashMap::from([("a".into(), 100), ("b".into(), 80)]));
        store.set_max_ages(HashMap::from([("a".into(), 60), ("b".into(), 60)]));
        store.apply_source_fields(&[(F::FlowGpm, 7.0)], 1_000, true, "a");
        store.apply_source_fields(&[(F::FlowGpm, 9.0)], 1_010, true, "b");
        let mut selected = store.flow_readout(1_010);
        assert_eq!(selected.rate_gpm, Some(7.0));
        assert_eq!(selected.rate_source_id.as_deref(), Some("source:a"));
        assert_eq!(
            selected.total_gal_today, None,
            "rate alone cannot invent a total"
        );
        selected.prefer_controller("controller", None);
        assert_eq!(
            selected.rate_gpm,
            Some(7.0),
            "unavailable controller leaves bus selected"
        );
        selected.prefer_controller("controller", Some(0.0));
        assert_eq!(
            selected.rate_gpm,
            Some(0.0),
            "real idle controller meter outranks bus"
        );
        assert_eq!(
            selected.rate_source_id.as_deref(),
            Some("controller:controller")
        );
        assert_eq!(selected.total_gal_today, None);

        assert_eq!(
            store.flow_readout(999).rate_gpm,
            None,
            "future readings are unavailable"
        );
        assert_eq!(
            store.flow_readout(1_061).rate_gpm,
            None,
            "stale readings are unavailable"
        );
        store.apply_source_fields(
            &[(F::FlowGpm, 9.0), (F::FlowTotalGalToday, 42.0)],
            1_062,
            true,
            "b",
        );
        let fallback = store.flow_readout(1_062);
        assert_eq!(fallback.rate_gpm, Some(9.0));
        assert_eq!(fallback.rate_source_id.as_deref(), Some("source:b"));
        assert_eq!(fallback.total_gal_today, Some(42.0));
        store.invalidate_flow_source("b");
        assert_eq!(
            store.flow_readout(1_062),
            crate::model::FlowReadout::default()
        );
        store.apply_source_fields(&[(F::FlowGpm, 2.0)], 1_063, true, "a");
        assert_eq!(store.flow_readout(1_063).rate_gpm, Some(2.0));
    }

    #[test]
    fn future_incumbents_cannot_block_current_evidence_in_either_tier() {
        for live in [true, false] {
            let store = LiveWeatherStore::new();
            store.set_priorities(HashMap::from([
                ("future".into(), 100),
                ("current".into(), 80),
            ]));
            store.apply_source_fields(&[(F::FlowGpm, 20.0)], 2_000, live, "future");
            assert_eq!(store.flow_readout(1_000).rate_gpm, None);
            store.apply_source_fields(&[(F::FlowGpm, 3.0)], 1_000, live, "current");
            assert_eq!(store.flow_readout(1_000).rate_gpm, Some(3.0));
        }
        let blank = LiveWeatherStore::new();
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            blank.apply_source_fields(&[(F::FlowGpm, invalid)], 1_000, true, "invalid");
            assert_eq!(blank.flow_readout(1_000).rate_gpm, None);
        }
    }

    #[test]
    fn flow_disconnect_releases_flow_pins_without_erasing_other_weather() {
        let store = LiveWeatherStore::new();
        store.set_priorities(HashMap::from([
            ("primary".into(), 100),
            ("backup".into(), 80),
        ]));
        store.set_field_overrides(HashMap::from([("flow_gpm", "primary".into())]));
        store.apply_source_fields(
            &[(F::FlowGpm, 9.0), (F::AirTempF, 70.0)],
            1_000,
            true,
            "primary",
        );
        store.invalidate_flow_source("primary");
        store.apply_source_fields(
            &[(F::FlowGpm, 3.0), (F::AirTempF, 80.0)],
            1_001,
            true,
            "backup",
        );
        assert_eq!(store.flow_readout(1_001).rate_gpm, Some(3.0));
        assert_eq!(
            store.flow_readout(1_001).rate_source_id.as_deref(),
            Some("source:backup")
        );
        assert_eq!(store.snapshot().air_temp_f, 70.0);
    }

    #[test]
    fn a_station_respects_a_higher_priority_owner_and_reclaims_when_it_goes_stale() {
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("ecowitt".to_string(), 70);
        p.insert("tempest".to_string(), 50);
        store.set_priorities(p);
        store.apply_source_fields(&[(F::AirTempF, 60.0)], 1_000, true, "ecowitt");
        // The station (prio 50) reports while Ecowitt (70) is fresh -> suppressed.
        store.apply_source_fields(&[(F::AirTempF, 86.0)], 1_010, true, "tempest");
        let s = store.snapshot();
        assert_eq!(
            s.source_label, "ecowitt",
            "a lower-priority station must not seize a fresh higher owner"
        );
        assert!((s.air_temp_f - 60.0).abs() < 0.01);
        // After the owner goes stale (> LIVE_FRESHNESS_SECS), it reclaims.
        store.apply_source_fields(&[(F::AirTempF, 68.0)], 1_010 + 601, true, "tempest");
        let s2 = store.snapshot();
        assert_eq!(
            s2.source_label, "tempest",
            "the station reclaims after the owner is stale"
        );
        assert!((s2.air_temp_f - 68.0).abs() < 0.01);
    }

    #[test]
    fn stale_owner_yields_current_to_other_live_source() {
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("ecowitt".to_string(), 70);
        p.insert("davis".to_string(), 60);
        store.set_priorities(p);
        store.apply_source_fields(&[(F::AirTempF, 60.0)], 1_000, true, "ecowitt");
        // Davis (lower) takes over only after the owner goes stale.
        store.apply_source_fields(&[(F::AirTempF, 80.0)], 1_000 + 601, true, "davis");
        assert_eq!(store.snapshot().air_temp_f, 80.0);
        assert_eq!(store.snapshot().source_label, "davis");
    }

    #[test]
    fn forecast_fills_display_once_live_station_is_stale() {
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 1_000, true, "station"); // live station
                                                                                   // Forecast far in the future (> LIVE_FRESHNESS_SECS later): station is
                                                                                   // stale, so forecast may take over the display.
        store.apply_source_fields(&[(F::AirTempF, 50.0)], 1_000 + 601, false, "forecast");
        assert_eq!(store.snapshot().air_temp_f, 50.0);
    }

    // ── Per-field user overrides (the Data sources page) ──────────────────────

    fn overrides_of(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    #[test]
    fn override_makes_chosen_source_win_a_field() {
        // The owner ask: pin WIND to the LOWER-priority Tempest; it must beat the
        // higher-priority Ecowitt for wind regardless of priority, while Ecowitt
        // still owns the un-overridden fields (no collateral hijack).
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 60);
        p.insert("ecowitt".to_string(), 90); // higher priority
        store.set_priorities(p);
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "tempest")]));

        // Tempest (pinned) claims wind first.
        store.apply_source_fields(&[(F::WindMph, 5.0)], 1_000, true, "tempest");
        // Ecowitt (higher priority) writes wind + temp on the next tick. Without
        // the override its 90 would seize wind; the override blocks it for wind
        // only, so wind stays Tempest's 5.0 while temp follows Ecowitt.
        store.apply_source_fields(
            &[(F::WindMph, 22.0), (F::AirTempF, 71.0)],
            1_010,
            true,
            "ecowitt",
        );
        let s = store.snapshot();
        assert_eq!(
            s.wind_avg_mph, 5.0,
            "override pins wind to the chosen source"
        );
        assert_eq!(s.air_temp_f, 71.0, "un-overridden field follows priority");

        // The pinned source keeps winning wind on its own refreshes.
        store.apply_source_fields(&[(F::WindMph, 6.0)], 1_020, true, "tempest");
        assert_eq!(store.snapshot().wind_avg_mph, 6.0);
    }

    #[test]
    fn override_source_offline_falls_back_to_priority_no_data_loss() {
        // Safety-adjacent invariant: if the PINNED source has no recent value,
        // the override must NOT blank the field. Another live source fills it via
        // the normal priority merge so the engine never loses a reading.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 60);
        p.insert("ecowitt".to_string(), 90);
        store.set_priorities(p);
        // Pin wind to "tempest" -- but tempest never reports in this test.
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "tempest")]));

        // Only Ecowitt reports. The pinned source has never been seen, so the
        // override yields to priority and Ecowitt's value is taken (not blanked).
        store.apply_source_fields(&[(F::WindMph, 22.0)], 1_000, true, "ecowitt");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            22.0,
            "an offline pinned source must fall back to priority, never blank the field"
        );

        // Now the pinned source comes online far later: it reclaims the field.
        store.apply_source_fields(&[(F::WindMph, 4.0)], 1_000 + 5_000, true, "tempest");
        assert_eq!(store.snapshot().wind_avg_mph, 4.0);

        // ...and a non-pinned source writing while the pinned owner is STALE
        // (no recent value) again falls back to priority rather than losing data.
        store.apply_source_fields(&[(F::WindMph, 30.0)], 1_000 + 5_000 + 601, true, "ecowitt");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            30.0,
            "a stale pinned owner yields back to priority"
        );
    }

    #[test]
    fn empty_overrides_are_byte_identical_to_no_override() {
        // Parity pin: with NO overrides installed, an identical sequence of
        // applies produces a byte-identical snapshot to the same sequence run on
        // a store that never had overrides touched at all. Guards the additive
        // contract (no override == exact current behavior).
        // Snapshot has no PartialEq; compare the serialized JSON, which is the
        // exact "byte-identical" contract the public API + UI consume anyway.
        let run = |install_empty: bool| -> String {
            let store = LiveWeatherStore::new();
            let mut p = HashMap::new();
            p.insert("ecowitt".to_string(), 90);
            p.insert("davis".to_string(), 60);
            store.set_priorities(p);
            if install_empty {
                store.set_field_overrides(HashMap::new()); // explicit empty map
            }
            store.apply_source_fields(
                &[(F::AirTempF, 70.0), (F::RhPct, 55.0), (F::WindMph, 8.0)],
                1_000,
                true,
                "ecowitt",
            );
            store.apply_source_fields(&[(F::AirTempF, 99.0)], 1_010, true, "davis");
            store.apply_source_fields(&[(F::PressureInHg, 29.9)], 1_020, false, "forecast");
            serde_json::to_string(&*store.snapshot()).unwrap()
        };
        assert_eq!(
            run(true),
            run(false),
            "an empty override map must merge byte-identically to never installing one"
        );
    }

    // ── Per-field LIVE-rain freshness (rain_live_epoch) ───────────────────────

    #[test]
    fn stale_forecast_rain_with_fresh_barometer_is_not_live_rain() {
        // The regression guard: Open-Meteo current (a forecast fill,
        // live_current=false) writes rain_intensity_in_hr, while a barometer-only
        // LIVE source keeps last_packet_epoch fresh. The whole-snapshot freshness
        // would mislabel the stale cloud rain as a live station rate and could
        // hard-skip a dry day. With a per-field rain_live_epoch set ONLY by live
        // writers, the engine sees NO live current-rain here.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("open_meteo".to_string(), 50);
        p.insert("baro".to_string(), 100);
        store.set_priorities(p);

        // Open-Meteo current fills rain_intensity (forecast, live_current=false).
        store.apply_source_fields(&[(F::RainIntensityInHr, 0.30)], 1_000, false, "open_meteo");
        // A barometer-only LIVE source keeps the snapshot fresh, but provides NO
        // rain field, so it must not stamp the live-rain epoch.
        store.apply_source_fields(&[(F::PressureInHg, 29.95)], 1_005, true, "baro");

        let s = store.snapshot();
        // Display still shows the cloud rain rate (forecast fill is visible)...
        assert_eq!(s.rain_intensity_in_hr, 0.30);
        // ...and the barometer made the SNAPSHOT fresh...
        assert_eq!(s.last_packet_epoch, 1_005, "barometer keeps snapshot fresh");
        // ...but NO live source ever wrote rain, so the per-field live-rain epoch
        // stays 0: the engine's "currently raining" gate sees no live rain.
        assert_eq!(
            s.rain_live_epoch, 0,
            "a forecast-filled rain rate must never read as live station rain"
        );
    }

    #[test]
    fn live_rain_write_stamps_rain_live_epoch() {
        // A genuine live source reporting rain DOES advance rain_live_epoch, so
        // the engine trusts a real station rain rate.
        let store = LiveWeatherStore::new();
        store.apply_source_fields(&[(F::RainIntensityInHr, 0.12)], 2_000, true, "station");
        let s = store.snapshot();
        assert_eq!(s.rain_intensity_in_hr, 0.12);
        assert_eq!(
            s.rain_live_epoch, 2_000,
            "a live rain write stamps the per-field live-rain epoch"
        );
    }

    // ── Open-Meteo LIVE current conditions into the merge ─────────────────────

    #[test]
    fn open_meteo_current_emits_wind_into_merge_as_low_priority_fallback() {
        // DEFAULT (no override): a LAN station owns wind by the live-vs-forecast
        // distinction, and Open-Meteo current (a cloud source emitted with
        // live_current=false) is a forecast-FILL fallback. It writes wind only
        // when no fresh live source owns it; a fresh station keeps wind. This is
        // the "merge unchanged by default" invariant.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100);
        p.insert("open_meteo".to_string(), 50); // lower, cloud current
        store.set_priorities(p);

        // Open-Meteo current arrives first (live_current=false): with no live
        // owner yet, it FILLS wind so the dashboard isn't blank pre-station.
        store.apply_source_fields(&[(F::WindMph, 12.0)], 1_000, false, "open_meteo");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            12.0,
            "Open-Meteo current fills wind when no live station owns it"
        );

        // A live LAN station reports wind: it claims ownership (live beats fill).
        store.apply_source_fields(&[(F::WindMph, 5.0)], 1_010, true, "tempest");
        assert_eq!(store.snapshot().wind_avg_mph, 5.0, "live station owns wind");

        // Open-Meteo current refreshes moments later: it must NOT overwrite the
        // fresh live station (forecast_may_fill is blocked while the owner is
        // fresh). The merge is unchanged from the station-only case.
        store.apply_source_fields(&[(F::WindMph, 22.0)], 1_020, false, "open_meteo");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            5.0,
            "Open-Meteo current must not clobber a fresh live station by default"
        );
    }

    #[test]
    fn override_pins_wind_to_open_meteo_over_higher_priority_station() {
        // The owner ask: "my wind should be cloud-sourced, not my Tempest."
        // Pin WIND = open_meteo. Even though the Tempest is a higher-priority
        // LIVE station, the override makes Open-Meteo current (live_current=false)
        // own WindMph, while the Tempest still owns every un-pinned field.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100); // higher-priority LIVE station
        p.insert("open_meteo".to_string(), 50); // lower-priority cloud current
        store.set_priorities(p);
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "open_meteo")]));

        // Open-Meteo current (pinned) reports wind: the override forces the claim
        // even though it's a forecast-fill source, and stamps its freshness.
        store.apply_source_fields(&[(F::WindMph, 14.0)], 1_000, false, "open_meteo");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            14.0,
            "pinned Open-Meteo wins wind"
        );

        // The higher-priority live Tempest writes wind + temp: the override blocks
        // it for WIND (Open-Meteo stays the owner) while temp follows the station.
        store.apply_source_fields(
            &[(F::WindMph, 3.0), (F::AirTempF, 88.0)],
            1_010,
            true,
            "tempest",
        );
        let s = store.snapshot();
        assert_eq!(
            s.wind_avg_mph, 14.0,
            "override pins wind to Open-Meteo even over a higher-priority station"
        );
        assert_eq!(
            s.air_temp_f, 88.0,
            "un-pinned field still follows the live station"
        );

        // Open-Meteo's next current refresh keeps owning wind.
        store.apply_source_fields(&[(F::WindMph, 16.0)], 1_020, false, "open_meteo");
        assert_eq!(store.snapshot().wind_avg_mph, 16.0);
    }

    // ── Per-source max_age + cloud fallback chain + tier lock (fixes #2/#3/#4) ──

    #[test]
    fn pinned_cloud_keeps_wind_past_600s_against_a_fast_tempest() {
        // The owner's WIND BUG: pin WIND = open_meteo with a per-source
        // max_age of 2100s (its ~1800s refresh cadence). A 60s-cadence Tempest
        // writes between Open-Meteo refreshes. With the OLD hardcoded 600s window
        // the pinned cloud was judged stale at the 600s mark and the wind-shadowed
        // Tempest reclaimed wind; with the per-source max_age it stays the owner.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100); // higher-priority LIVE station
        p.insert("open_meteo".to_string(), 50); // lower-priority cloud current
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("open_meteo".to_string(), 2100); // honor the 1800s cadence
        store.set_max_ages(ages);
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "open_meteo")]));

        // Open-Meteo current (pinned) reports wind at t=1000 (forecast fill).
        store.apply_source_fields(&[(F::WindMph, 14.0)], 1_000, false, "open_meteo");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            14.0,
            "pinned cloud owns wind"
        );

        // The fast Tempest writes wind at t=1700 (700s later -> PAST the old 600s
        // window, but WITHIN the cloud's 2100s max_age). The override must still
        // block the live station for wind.
        store.apply_source_fields(&[(F::WindMph, 3.0)], 1_700, true, "tempest");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            14.0,
            "a 1800s-cadence pinned cloud keeps wind past 600s against a 60s Tempest"
        );

        // The cloud refreshes within its window: it keeps owning wind.
        store.apply_source_fields(&[(F::WindMph, 16.0)], 1_800, false, "open_meteo");
        assert_eq!(store.snapshot().wind_avg_mph, 16.0);
    }

    #[test]
    fn stale_pinned_cloud_demotes_through_clouds_then_live_tempest_last_resort() {
        // TIER LOCK with a last-resort backup, and the cloud fallback chain:
        // pin WIND to a CLOUD. While ANY cloud in the chain is fresh, the field
        // demotes DOWN the cloud chain and the wind-shadowed live Tempest stays
        // blocked. Once the WHOLE cloud tier is stale, the Tempest reclaims as the
        // LAST RESORT (a reading beats "no backup"), and the pinned cloud reclaims
        // the moment it refreshes.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100); // highest-priority LIVE station
        p.insert("open_meteo".to_string(), 50); // pinned cloud
        p.insert("nws".to_string(), 40); // next cloud in the chain
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("open_meteo".to_string(), 2100);
        ages.insert("nws".to_string(), 2100);
        store.set_max_ages(ages);
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "open_meteo")]));

        // The pinned cloud establishes ownership at t=1000.
        store.apply_source_fields(&[(F::WindMph, 14.0)], 1_000, false, "open_meteo");
        assert_eq!(store.snapshot().wind_avg_mph, 14.0);

        // The pinned cloud goes stale; the NEXT cloud (NWS) fills the field. It
        // demotes DOWN the cloud chain, not to the wind-shadowed live Tempest.
        store.apply_source_fields(&[(F::WindMph, 9.0)], 1_000 + 2_200, false, "nws");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            9.0,
            "a stale pinned cloud demotes to the next cloud in the fill chain"
        );

        // While NWS (the demoted cloud) is still FRESH, the live Tempest cannot
        // reclaim: the cloud tier is alive, so the lock holds.
        store.apply_source_fields(&[(F::WindMph, 3.0)], 1_000 + 2_300, true, "tempest");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            9.0,
            "the live tier cannot reclaim while a cloud is still freshly filling the field"
        );

        // NWS ALSO goes stale: the WHOLE cloud tier is now exhausted. The live
        // Tempest reclaims wind as the LAST RESORT (better a reading than nothing).
        store.apply_source_fields(&[(F::WindMph, 2.0)], 1_000 + 2_200 + 2_200, true, "tempest");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            2.0,
            "once every cloud is stale the wind-shadowed Tempest is the last resort"
        );

        // The pinned cloud refreshes: it reclaims the field at once (the pin
        // re-engages over the last-resort live station, the whole point of pinning).
        store.apply_source_fields(
            &[(F::WindMph, 11.0)],
            1_000 + 2_200 + 2_300,
            false,
            "open_meteo",
        );
        assert_eq!(
            store.snapshot().wind_avg_mph,
            11.0,
            "the pinned cloud reclaims the moment it refreshes"
        );
    }

    #[test]
    fn stale_whole_cloud_tier_lets_the_live_tempest_reclaim_as_last_resort() {
        // LAST-RESORT BACKUP: when a pinned cloud AND every fallback cloud have all
        // gone stale (the whole cloud tier for the field is exhausted), the field
        // must NOT stay pinned to a dead cloud. The wind-shadowed Tempest reclaims
        // it as the last resort. The pin still sticks: a fresh cloud refill re-wins.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 100); // live station (wind-shadowed)
        p.insert("open_meteo".to_string(), 50); // pinned cloud
        p.insert("nws".to_string(), 40); // fallback cloud
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("open_meteo".to_string(), 2100);
        ages.insert("nws".to_string(), 2100);
        store.set_max_ages(ages);
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "open_meteo")]));

        // The pinned cloud establishes ownership of wind at t=1000.
        store.apply_source_fields(&[(F::WindMph, 14.0)], 1_000, false, "open_meteo");
        assert_eq!(store.snapshot().wind_avg_mph, 14.0);

        // The fallback cloud also fills it once, so the fill tier knows NWS.
        store.apply_source_fields(&[(F::WindMph, 9.0)], 1_000 + 2_300, false, "nws");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            9.0,
            "cloud tier still owns it"
        );

        // FAR past BOTH clouds' 2100s max_age (no cloud has refilled): the whole
        // cloud tier is stale, so the live Tempest reclaims wind as the last resort.
        let t_exhausted = 1_000 + 2_300 + 2_200;
        store.apply_source_fields(&[(F::WindMph, 3.0)], t_exhausted, true, "tempest");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            3.0,
            "an exhausted cloud tier yields to the live station as the last resort"
        );

        // The pin still sticks: the pinned cloud comes back, and it re-wins wind.
        store.apply_source_fields(&[(F::WindMph, 11.0)], t_exhausted + 10, false, "open_meteo");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            11.0,
            "a refreshed pinned cloud re-claims the field (the pin never dissolves)"
        );

        // And once the cloud is fresh again, the tier lock re-engages: the Tempest
        // is blocked once more.
        store.apply_source_fields(&[(F::WindMph, 2.0)], t_exhausted + 20, true, "tempest");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            11.0,
            "the tier lock re-engages while the pinned cloud is fresh again"
        );
    }

    #[test]
    fn higher_priority_cloud_wins_the_fill_over_a_lower_one() {
        // CLOUD FALLBACK CHAIN, a priority-aware fill: with NO override and
        // no live station, two clouds compete for a field's fill by priority. The
        // higher-priority cloud must win, and a lower one must not steal it just by
        // writing last (the old staleness-only fill let the last writer win).
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("open_meteo".to_string(), 70); // higher-priority cloud
        p.insert("nws".to_string(), 40); // lower-priority cloud
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("open_meteo".to_string(), 2100);
        ages.insert("nws".to_string(), 2100);
        store.set_max_ages(ages);

        // Lower cloud fills first (nothing owns the fill yet).
        store.apply_source_fields(&[(F::WindMph, 9.0)], 1_000, false, "nws");
        assert_eq!(store.snapshot().wind_avg_mph, 9.0, "first cloud fills wind");

        // Higher-priority cloud writes: it WINS the fill chain.
        store.apply_source_fields(&[(F::WindMph, 14.0)], 1_010, false, "open_meteo");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            14.0,
            "the higher-priority cloud wins the fill"
        );

        // The lower cloud writes again while the higher is fresh: it must NOT
        // steal the fill back (strict `>` + fresh higher owner).
        store.apply_source_fields(&[(F::WindMph, 30.0)], 1_020, false, "nws");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            14.0,
            "a lower-priority cloud cannot reclaim the fill from a fresh higher cloud"
        );
    }

    // ── MRMS per-field rain-RATE staleness (plan section 1.3 / 1.5) ───────────

    #[test]
    fn unpinned_stale_mrms_rain_rate_demotes_to_open_meteo() {
        // The exact MRMS-no-coverage case, with NO override pin (the unpinned
        // priority-fill demote that was unverified before). MRMS (priority 75,
        // wide 7200s source window) owns the rain RATE fill. It then goes silent
        // (no-coverage at the coast). Once the clock advances past the TIGHT
        // per-field rate window (MAX_AGE_MRMS_RATE_S = 900s), the lower-priority
        // Open-Meteo (50) must take rain_intensity_in_hr, even though 7200s has not
        // elapsed: the rate is judged on its own ~15 min cadence, not the wide
        // accumulation window. The display never blanks.
        use crate::config::region::{MAX_AGE_MRMS_RATE_S, MAX_AGE_MRMS_S};
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        store.set_rain_natures(HashMap::from([(
            MRMS_WRITER_LABEL.into(),
            crate::model::RainNature::RadarQpe,
        )]));
        p.insert(MRMS_WRITER_LABEL.to_string(), 75); // radar QPE, US authority
        p.insert("open_meteo".to_string(), 50); // model backstop
        store.set_priorities(p);
        // Source-level windows exactly as the region seeder stamps them: MRMS gets
        // the wide 7200s accumulation window, Open-Meteo the 2100s slow cadence.
        let mut ages = HashMap::new();
        ages.insert(MRMS_WRITER_LABEL.to_string(), MAX_AGE_MRMS_S as i32);
        ages.insert("open_meteo".to_string(), 2100);
        store.set_max_ages(ages);

        // MRMS owns the rain rate fill at t=1000 (radar measured the cell).
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.20)],
            1_000,
            false,
            MRMS_WRITER_LABEL,
        );
        assert_eq!(store.snapshot().rain_intensity_in_hr, 0.20);
        let owner0 = store.rain_owner(1_010).expect("MRMS owns the rain rate");
        assert_eq!(owner0.label, MRMS_WRITER_LABEL);
        assert!(
            owner0.is_fresh,
            "MRMS is fresh right after it filled the rate"
        );

        // Open-Meteo writes BEFORE the rate window elapses (e.g. 10 min later):
        // MRMS (75) is still fresh on the rate, so the lower Open-Meteo (50) must
        // NOT steal the rain fill yet (the chain only demotes on stale or strict >).
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.00)],
            1_000 + 600,
            false,
            "open_meteo",
        );
        assert_eq!(
            store.snapshot().rain_intensity_in_hr,
            0.20,
            "Open-Meteo must not steal a still-fresh MRMS rain rate"
        );
        assert_eq!(
            store.rain_owner(1_000 + 600).unwrap().label,
            MRMS_WRITER_LABEL,
            "MRMS still owns the rate inside its 900s window"
        );

        // Now advance PAST the tight per-field rate window (900s) with MRMS silent.
        // Open-Meteo's write demotes MRMS off the rate (the per-field window, not
        // the 7200s accumulation window, governs the rate), and rain_owner reports
        // Open-Meteo, fresh. This happens at ~15 min, NOT up to 2 hr.
        let t_demote = 1_000 + MAX_AGE_MRMS_RATE_S as i64 + 60;
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.05)],
            t_demote,
            false,
            "open_meteo",
        );
        assert_eq!(
            store.snapshot().rain_intensity_in_hr,
            0.05,
            "Open-Meteo takes the rain rate once the MRMS rate is stale past 900s"
        );
        let owner1 = store
            .rain_owner(t_demote)
            .expect("a source still owns the rain rate");
        assert_eq!(
            owner1.label, "open_meteo",
            "rain_owner reports Open-Meteo after the MRMS rate falls through"
        );
        assert!(
            owner1.is_fresh,
            "the Open-Meteo rain owner is fresh on its own 2100s window"
        );
        // The fall-through happened well before the wide MRMS source window: the
        // per-field split, not a blanket window cut, is what demoted the rate.
        assert!(
            t_demote - 1_000 < MAX_AGE_MRMS_S as i64,
            "the demote happens inside the 2 hr accumulation window (per-field split)"
        );
    }

    #[test]
    fn mrms_accumulation_field_stays_fresh_while_rate_demotes() {
        // Companion to the test above: under the SAME clock advance that demotes
        // the MRMS rain RATE, the gauge-corrected hourly ACCUMULATION field
        // (rain_today_in / the rain_in_today snapshot key) must STILL be owned by
        // MRMS, because the accumulation keeps the wide 7200s window. This proves
        // the per-field split (rate tight, accumulation wide), not a blanket cut.
        use crate::config::region::{MAX_AGE_MRMS_RATE_S, MAX_AGE_MRMS_S};
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        store.set_rain_natures(HashMap::from([(
            MRMS_WRITER_LABEL.into(),
            crate::model::RainNature::RadarQpe,
        )]));
        p.insert(MRMS_WRITER_LABEL.to_string(), 75);
        p.insert("open_meteo".to_string(), 50);
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert(MRMS_WRITER_LABEL.to_string(), MAX_AGE_MRMS_S as i32);
        ages.insert("open_meteo".to_string(), 2100);
        store.set_max_ages(ages);

        // MRMS fills BOTH products at t=1000: the rate and the accumulation total.
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.20), (F::RainTodayIn, 0.85)],
            1_000,
            false,
            MRMS_WRITER_LABEL,
        );
        // Provenance keys the accumulation under the WeatherField config name.
        let prov0 = store.field_source_map();
        assert_eq!(
            prov0.get("rain_today_in").map(String::as_str),
            Some(MRMS_WRITER_LABEL),
            "MRMS owns the accumulation field when it first fills it"
        );

        // Advance PAST the tight rate window so the rate demotes, and let Open-Meteo
        // write its rate only (a model has no gauge-corrected hourly accumulation,
        // so it does not contend for rain_today_in). The accumulation is still well
        // inside the wide 7200s MRMS window.
        let t_after_rate = 1_000 + MAX_AGE_MRMS_RATE_S as i64 + 60;
        assert!(
            t_after_rate - 1_000 < MAX_AGE_MRMS_S as i64,
            "the clock advance stays inside the wide accumulation window"
        );
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.05)],
            t_after_rate,
            false,
            "open_meteo",
        );

        // The rate fell through to Open-Meteo...
        assert_eq!(
            store.rain_owner(t_after_rate).unwrap().label,
            "open_meteo",
            "the rain rate demoted to Open-Meteo"
        );
        // ...but the ACCUMULATION field is still owned by MRMS (wide window intact).
        let prov1 = store.field_source_map();
        assert_eq!(
            prov1.get("rain_today_in").map(String::as_str),
            Some(MRMS_WRITER_LABEL),
            "the MRMS accumulation field keeps its wide 7200s window while the rate demotes"
        );
    }

    // ── Ownership follows the writer: field_owners never keeps naming a ─────
    // ── displaced live station for a value a cloud produced ─────────────────

    #[test]
    fn pinned_cloud_rain_total_reports_the_cloud_as_the_owner() {
        // THE PROVENANCE DEFECT. Rain-today is PINNED to a cloud model while a
        // real gauge is on the LAN and still inside its freshness window. The pin
        // is honored for the VALUE (that is what a pin is for), so the number in
        // the snapshot is the model's. Ownership has to follow it: the surface
        // that answers "where is this reading coming from" must say open_meteo,
        // is_live=false, or the observations ledger tags a MODELLED day total
        // "gauge" under the station's name and day-maxes it above the model rain
        // archive for the whole balance window.
        //
        // Before the fix, apply_source_fields recorded the pinned cloud in
        // `fill_owners` and left the live entry in `field_owners` untouched.
        // `rain_today_owner` reads the live tier FIRST and returns early while it
        // still looks fresh, so it answered ("ecowitt", is_live: true) for a
        // value ecowitt did not write.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("ecowitt".to_string(), 90); // real gauge on the LAN
        p.insert("open_meteo".to_string(), 50); // model day total
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("ecowitt".to_string(), 600); // 1 min cadence, 10 min window
        ages.insert("open_meteo".to_string(), 2100); // ~30 min cadence
        store.set_max_ages(ages);
        store.set_field_overrides(overrides_of(&[("rain_in_today", "open_meteo")]));

        // t=1000: the gauge writes the day's total first (the pinned cloud has
        // not reported yet, so the chain is never-seen and the live merge takes
        // it).
        store.apply_source_fields(&[(F::RainTodayIn, 0.42)], 1_000, true, "ecowitt");
        let owner0 = store
            .rain_today_owner(1_000)
            .expect("the gauge owns the day total");
        assert_eq!(owner0.label, "ecowitt");
        assert!(
            owner0.is_live && owner0.is_fresh,
            "a real gauge that just wrote the total IS the fresh live owner"
        );

        // t=1100: the PINNED cloud writes its model day total, only 100s later,
        // well inside the gauge's own 600s window, so the displaced live owner
        // still looks fresh. The pin hands the cloud the field regardless.
        store.apply_source_fields(&[(F::RainTodayIn, 0.61)], 1_100, false, "open_meteo");
        assert_eq!(
            store.snapshot().rain_in_today,
            0.61,
            "the pin gives the model day total to the snapshot"
        );

        // ...so the OWNER of that number must be the cloud that produced it.
        let owner1 = store
            .rain_today_owner(1_100)
            .expect("some source owns the day total");
        assert_eq!(
            owner1.label, "open_meteo",
            "the pinned cloud wrote the day total, so it must be named as owner"
        );
        assert!(
            !owner1.is_live,
            "a model day total must not report as a live gauge reading"
        );
        assert!(owner1.is_fresh, "the cloud just wrote, on its own window");

        // The display provenance and the arbitration owner map now agree. They
        // contradicted each other before: provenance said open_meteo (it records
        // every allowed write) while field_owners still said ecowitt.
        assert_eq!(
            store
                .field_source_map()
                .get("rain_today_in")
                .map(String::as_str),
            Some("open_meteo"),
            "display provenance and the owner map must name the same source"
        );
    }

    #[test]
    fn chained_cloud_rain_total_reports_the_cloud_as_the_owner() {
        // The same defect through the CHAIN path rather than the single pin, and
        // with the ordering reversed: the chain is [open_meteo, ecowitt], so the
        // gauge is the BACKUP and legitimately owns the total while the cloud
        // primary is silent. When the primary reports it reclaims the field (it
        // is earlier in the chain) from a gauge that is still fresh, and
        // ownership has to move with the value.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("ecowitt".to_string(), 90);
        p.insert("open_meteo".to_string(), 50);
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("ecowitt".to_string(), 600);
        ages.insert("open_meteo".to_string(), 2100);
        store.set_max_ages(ages);
        store.set_field_chains(chains_of(&[("rain_in_today", &["open_meteo", "ecowitt"])]));

        // t=1000: the cloud primary has never reported, so the backup gauge owns
        // the total (ordered failover working as designed).
        store.apply_source_fields(&[(F::RainTodayIn, 0.42)], 1_000, true, "ecowitt");
        let owner0 = store.rain_today_owner(1_000).unwrap();
        assert_eq!(
            owner0.label, "ecowitt",
            "the backup gauge owns the total while the primary is silent"
        );

        // t=1100: the chain PRIMARY reports and reclaims the field from a gauge
        // that is still well inside its 600s window.
        store.apply_source_fields(&[(F::RainTodayIn, 0.61)], 1_100, false, "open_meteo");
        assert_eq!(store.snapshot().rain_in_today, 0.61);
        let owner1 = store.rain_today_owner(1_100).unwrap();
        assert_eq!(
            owner1.label, "open_meteo",
            "the chain primary reclaimed the field, so it owns the reading"
        );
        assert!(
            !owner1.is_live,
            "a chain-reclaimed cloud total is not a live gauge reading"
        );
    }

    #[test]
    fn pinned_cloud_et0_does_not_read_as_a_live_accumulator() {
        // The same lie on the ET0 field, where it costs water. A live station
        // reports et0_today as an accumulator SINCE MIDNIGHT; a cloud writes the
        // FULL-DAY forecast figure. `et0_today_is_live` is the ownership bit the
        // refresher uses (never a magnitude heuristic) to decide whether the bus
        // et0_today can honestly serve as eto_spent_today_mm.
        //
        // Before the fix, a cloud pinned to et0_today wrote the full-day figure
        // into the snapshot while the displaced live station stayed recorded in
        // field_owners and still looked fresh, so this returned TRUE: a whole
        // day's ET0 counted as already spent by mid-morning, overstating the
        // deficit and watering a garden that did not need it.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("davis".to_string(), 90);
        p.insert("open_meteo".to_string(), 50);
        store.set_priorities(p);
        let mut ages = HashMap::new();
        ages.insert("davis".to_string(), 600);
        ages.insert("open_meteo".to_string(), 2100);
        store.set_max_ages(ages);
        store.set_field_overrides(overrides_of(&[("et0_today", "open_meteo")]));

        // t=1000: the live console's since-midnight accumulator.
        store.apply_source_fields(&[(F::Et0Today, 2.0)], 1_000, true, "davis");
        assert!(
            store.et0_today_is_live(1_000),
            "a live station's accumulator IS live-owned"
        );

        // t=1100: the pinned cloud's FULL-DAY figure, inside the console's
        // window. Only the ownership clear stops that being read as ET0 already
        // spent today, which would overstate the deficit and over-water.
        store.apply_source_fields(&[(F::Et0Today, 5.6)], 1_100, false, "open_meteo");
        assert_eq!(store.snapshot().et0_today, 5.6, "the pin wins the value");
        assert!(
            !store.et0_today_is_live(1_100),
            "a cloud's full-day ET0 must not read as a live accumulator"
        );
    }

    #[test]
    fn override_field_source_map_reports_pinned_owner() {
        // The snapshot's per-field provenance reflects the override owner so the
        // UI shows "Wind: tempest".
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 60);
        p.insert("ecowitt".to_string(), 90);
        store.set_priorities(p);
        store.set_field_overrides(overrides_of(&[("wind_avg_mph", "tempest")]));
        store.apply_source_fields(&[(F::WindMph, 5.0)], 1_000, true, "tempest");
        store.apply_source_fields(&[(F::WindMph, 22.0)], 1_010, true, "ecowitt");
        let map = store.field_source_map();
        assert_eq!(map.get("wind_mph").map(String::as_str), Some("tempest"));
    }

    // ── Per-field PRIORITY CHAINS (ordered backup failover) ───────────────────

    fn chains_of(pairs: &[(&'static str, &[&str])]) -> HashMap<&'static str, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (*k, v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn empty_chains_are_byte_identical_to_no_chain() {
        // Regression / additive contract: installing an EMPTY chain map must merge
        // byte-identically to never touching chains at all. Same JSON-serialized
        // snapshot contract the empty-override parity test uses.
        let run = |install_empty: bool| -> String {
            let store = LiveWeatherStore::new();
            let mut p = HashMap::new();
            p.insert("ecowitt".to_string(), 90);
            p.insert("davis".to_string(), 60);
            store.set_priorities(p);
            if install_empty {
                store.set_field_chains(HashMap::new()); // explicit empty chain map
            }
            store.apply_source_fields(
                &[(F::AirTempF, 70.0), (F::RhPct, 55.0), (F::WindMph, 8.0)],
                1_000,
                true,
                "ecowitt",
            );
            store.apply_source_fields(&[(F::AirTempF, 99.0)], 1_010, true, "davis");
            store.apply_source_fields(&[(F::PressureInHg, 29.9)], 1_020, false, "forecast");
            serde_json::to_string(&*store.snapshot()).unwrap()
        };
        assert_eq!(
            run(true),
            run(false),
            "an empty chain map must merge byte-identically to never installing one"
        );
    }

    #[test]
    fn multi_source_chain_orders_rain_failover_and_never_blanks() {
        // The headline chain behavior: rain chain [MRMS, NWS, OpenMeteo].
        //   (1) MRMS fresh  -> MRMS owns rain.
        //   (2) MRMS stale + NWS fresh -> NWS owns (ordered failover to the NEXT
        //       chain entry, NOT straight to priority).
        //   (3) whole chain stale -> falls through to the priority merge and a
        //       source writes the reading (NEVER blanked).
        // All three are cloud FILLS (live_current=false); the rain accumulation
        // field keeps the wide source window, so we drive freshness with explicit
        // max_ages and clock advances.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        store.set_rain_natures(HashMap::from([(
            MRMS_WRITER_LABEL.into(),
            crate::model::RainNature::RadarQpe,
        )]));
        p.insert(MRMS_WRITER_LABEL.to_string(), 75);
        p.insert("nws".to_string(), 70);
        p.insert("open_meteo".to_string(), 50);
        store.set_priorities(p);
        // Give each a 600s freshness window so staleness is easy to reason about.
        let mut ages = HashMap::new();
        ages.insert(MRMS_WRITER_LABEL.to_string(), 600);
        ages.insert("nws".to_string(), 600);
        ages.insert("open_meteo".to_string(), 600);
        store.set_max_ages(ages);
        store.set_field_chains(chains_of(&[(
            "rain_in_today",
            &[MRMS_WRITER_LABEL, "nws", "open_meteo"],
        )]));

        // (1) MRMS (primary) fills rain -> MRMS owns it, even though NWS/OpenMeteo
        // also write within the window.
        store.apply_source_fields(&[(F::RainTodayIn, 0.80)], 1_000, false, MRMS_WRITER_LABEL);
        // NWS writes while MRMS is fresh: blocked (later chain entry, earlier fresh).
        store.apply_source_fields(&[(F::RainTodayIn, 0.10)], 1_050, false, "nws");
        assert_eq!(
            store.snapshot().rain_in_today,
            0.80,
            "the fresh chain PRIMARY (MRMS) owns rain; a later entry is blocked"
        );
        assert_eq!(
            store
                .field_source_map()
                .get("rain_today_in")
                .map(String::as_str),
            Some(MRMS_WRITER_LABEL),
        );

        // (2) MRMS goes quiet. NWS (the NEXT chain entry) refreshes past the MRMS
        // window: it OWNS rain now (ordered failover), NOT Open-Meteo, NOT priority.
        store.apply_source_fields(&[(F::RainTodayIn, 0.12)], 1_000 + 700, false, "nws");
        assert_eq!(
            store.snapshot().rain_in_today,
            0.12,
            "MRMS stale + NWS fresh -> NWS (the next chain entry) owns rain"
        );
        assert_eq!(
            store
                .field_source_map()
                .get("rain_today_in")
                .map(String::as_str),
            Some("nws"),
            "ordered failover moved rain to NWS, not to priority"
        );

        // (3) The WHOLE chain goes stale. An off-chain source (a live station) then
        // writes rain: the chain must NOT blank it -- it falls through to the merge
        // and the station's reading is taken.
        store.apply_source_fields(
            &[(F::RainTodayIn, 0.30)],
            1_000 + 700 + 5_000,
            true,
            "backyard_gauge",
        );
        assert_eq!(
            store.snapshot().rain_in_today,
            0.30,
            "a fully-stale chain falls through to the priority merge; rain is NEVER blanked"
        );
    }

    #[test]
    fn one_element_chain_equals_the_equivalent_single_pin() {
        // A 1-element chain must behave byte-for-byte like the equivalent
        // field_source_overrides pin. Run the SAME apply sequence twice -- once
        // with a pin, once with a 1-element chain -- and compare the JSON snapshot.
        let run = |use_chain: bool| -> String {
            let store = LiveWeatherStore::new();
            let mut p = HashMap::new();
            p.insert("tempest".to_string(), 60);
            p.insert("ecowitt".to_string(), 90); // higher priority
            store.set_priorities(p);
            if use_chain {
                store.set_field_chains(chains_of(&[("wind_avg_mph", &["tempest"])]));
            } else {
                store.set_field_overrides(overrides_of(&[("wind_avg_mph", "tempest")]));
            }
            // Pinned/primary source claims wind; higher-priority Ecowitt is blocked
            // for wind but owns temp; the pinned source keeps winning on refresh.
            store.apply_source_fields(&[(F::WindMph, 5.0)], 1_000, true, "tempest");
            store.apply_source_fields(
                &[(F::WindMph, 22.0), (F::AirTempF, 71.0)],
                1_010,
                true,
                "ecowitt",
            );
            store.apply_source_fields(&[(F::WindMph, 6.0)], 1_020, true, "tempest");
            serde_json::to_string(&*store.snapshot()).unwrap()
        };
        assert_eq!(
            run(true),
            run(false),
            "a 1-element chain must merge byte-identically to the single pin"
        );
    }

    #[test]
    fn off_chain_source_blocked_while_chain_fresh_but_wins_when_chain_stale() {
        // A source NOT in the chain never owns while a chain entry is fresh, but CAN
        // win once the WHOLE chain is stale (the never-blank last resort). Chain is
        // [tempest, ecowitt] for wind; "davis" is off-chain.
        let store = LiveWeatherStore::new();
        let mut p = HashMap::new();
        p.insert("tempest".to_string(), 60);
        p.insert("ecowitt".to_string(), 70);
        p.insert("davis".to_string(), 100); // highest priority, but off-chain
        store.set_priorities(p);
        store.set_field_chains(chains_of(&[("wind_avg_mph", &["tempest", "ecowitt"])]));

        // Tempest (chain primary) claims wind.
        store.apply_source_fields(&[(F::WindMph, 5.0)], 1_000, true, "tempest");
        // Davis (off-chain, priority 100) writes while the chain is fresh: BLOCKED.
        store.apply_source_fields(&[(F::WindMph, 40.0)], 1_010, true, "davis");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            5.0,
            "an off-chain source is blocked while a chain entry is fresh"
        );

        // The chain goes stale (Tempest is the only entry that ever wrote, and it
        // is now well past its 600s window). Davis writes again: it WINS now (the
        // whole chain is stale), never blanking the field.
        store.apply_source_fields(&[(F::WindMph, 41.0)], 1_000 + 601, true, "davis");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            41.0,
            "an off-chain source wins once the whole chain is stale (never blank)"
        );
    }
}

#[cfg(all(test, feature = "ssr"))]
mod rain_provenance_review_tests {
    use super::*;
    use crate::model::RainNature;
    use crate::ports::weather_source::WeatherField as F;

    #[test]
    fn rain_kind_survives_renaming_and_stale_owners_do_not_borrow_a_fill() {
        let store = LiveWeatherStore::new();
        store.set_rain_natures(HashMap::from([
            ("yard-radar".into(), RainNature::RadarQpe),
            ("renamed-station".into(), RainNature::Measured),
            ("nws".into(), RainNature::Model),
        ]));
        store.set_max_ages(HashMap::from([("yard-radar".into(), 7200)]));
        store.apply_source_fields(&[(F::RainIntensityInHr, 0.2)], 1000, false, "yard-radar");
        assert_eq!(store.rain_owner(1001).unwrap().nature, RainNature::RadarQpe);
        assert!(!store.rain_owner(1901).unwrap().is_fresh);
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.0)],
            2000,
            false,
            "renamed-station",
        );
        assert_eq!(store.rain_owner(2001).unwrap().nature, RainNature::Measured);
        store.apply_source_fields(&[(F::RainIntensityInHr, 0.4)], 2700, false, "nws");
        assert_eq!(store.rain_owner(2701).unwrap().nature, RainNature::Model);
        store.apply_source_fields(&[(F::RainIntensityInHr, 0.0)], 2800, true, "gauge");
        let owner = store.rain_owner(3401).unwrap();
        assert_eq!(owner.label, "gauge");
        assert!(!owner.is_fresh);
        assert!(!store.rain_owner(2799).unwrap().is_fresh);
    }

    #[test]
    fn invalid_rain_cannot_replace_a_real_zero() {
        let store = LiveWeatherStore::new();
        store.apply_source_fields(
            &[(F::RainIntensityInHr, 0.0), (F::RainTodayIn, 0.0)],
            1000,
            true,
            "gauge",
        );
        for invalid in [-1.0, f64::NAN, f64::INFINITY] {
            store.apply_source_fields(
                &[(F::RainIntensityInHr, invalid), (F::RainTodayIn, invalid)],
                1001,
                true,
                "gauge",
            );
        }
        assert_eq!(store.snapshot().rain_intensity_in_hr, 0.0);
        assert_eq!(store.snapshot().rain_in_today, 0.0);
        assert!(!store.rain_owner(1601).unwrap().is_fresh);
    }
}

#[cfg(all(test, feature = "ssr"))]
mod current_evidence_alignment_tests {
    use super::*;
    use crate::engine::skip_rules::LiveReadings;
    use crate::ports::weather_source::WeatherField as F;

    #[test]
    fn wind_metrics_keep_independent_reports_and_missing_rapid_is_not_zero() {
        let store = LiveWeatherStore::new();
        store.apply_received_fields(&[(F::WindMph, 6.0)], 1000, 1000, true, "ha");
        assert!(!store
            .current_weather_samples(1000)
            .contains_key("rapid_wind_mph"));
        store.apply_received_fields(
            &[(F::RapidWindMph, 0.0), (F::WindLullMph, 0.0)],
            1001,
            1001,
            true,
            "station",
        );
        store.apply_received_fields(&[(F::WindMph, 7.0)], 1500, 1500, true, "ha");
        let samples = store.current_weather_samples(1602);
        assert_eq!(samples["wind_mph"].source_id, "ha");
        assert_eq!(samples["rapid_wind_mph"].observed_epoch, 1001);
        assert_eq!(samples["rapid_wind_mph"].value, 0.0);
        assert!(samples["rapid_wind_mph"].summary_at(1602).contains("stale"));
        assert!(samples["wind_lull_mph"].measured);
    }

    #[test]
    fn invalid_high_priority_current_fields_cannot_displace_valid_measurements() {
        let store = LiveWeatherStore::new();
        store.set_priorities(HashMap::from([("bad".into(), 100), ("good".into(), 50)]));
        store.apply_received_fields(
            &[(F::WindMph, 6.0), (F::RhPct, 50.0)],
            1000,
            1000,
            true,
            "good",
        );
        store.apply_received_fields(
            &[(F::WindMph, -1.0), (F::RhPct, 101.0)],
            1001,
            1001,
            true,
            "bad",
        );
        let samples = store.current_condition_samples(1001);
        assert_eq!(samples[1].as_ref().unwrap().source_id, "good");
        assert_eq!(samples[2].as_ref().unwrap().source_id, "good");
        assert_eq!(store.snapshot().wind_avg_mph, 6.0);
        assert_eq!(store.snapshot().rh_pct, 50.0);
    }

    #[test]
    fn selected_remote_measurement_drives_engine_and_older_model_cannot_steal_it() {
        let store = LiveWeatherStore::new();
        let now = 1_700_000_100;
        store.set_observed_condition_fields(HashMap::from([("remote-station".into(), [true; 3])]));
        store.set_max_ages(HashMap::from([
            ("remote-station".into(), 2100),
            ("model".into(), 2100),
        ]));
        store.set_field_chains(HashMap::from([(
            "wind_avg_mph",
            vec!["remote-station".into(), "yard".into(), "model".into()],
        )]));
        store.apply_received_fields(
            &[(F::AirTempF, 80.0), (F::RhPct, 60.0)],
            now,
            now,
            true,
            "yard",
        );
        store.apply_received_fields(
            &[(F::WindMph, 4.6)],
            now - 300,
            now,
            false,
            "remote-station",
        );
        store.apply_received_fields(&[(F::WindMph, 8.3)], now - 600, now + 1, false, "model");
        let samples = store.current_condition_samples(now + 1);
        assert_eq!(samples[1].as_ref().unwrap().source_id, "remote-station");
        assert!(samples[1]
            .as_ref()
            .unwrap()
            .summary_at(now)
            .contains("measured · 5m old"));
        let forecast = crate::forecast::snapshot::HourlyEntry {
            wind_mph: Some(22.0),
            ..Default::default()
        };
        let result = crate::assembly::readings::resolve_current_conditions(
            &samples,
            Some(&forecast),
            now + 1,
        );
        assert_eq!(result, (80.0, 4.6, 60.0, LiveReadings::Station));
        assert_eq!(
            store.snapshot().wind_live_epoch,
            0,
            "remote measurement is not a LAN packet"
        );
        // Shortening the source age takes effect in both arbitration and engine.
        store.set_max_ages(HashMap::from([("remote-station".into(), 60)]));
        let samples = store.current_condition_samples(now + 1);
        assert_eq!(
            crate::assembly::readings::resolve_current_conditions(
                &samples,
                Some(&forecast),
                now + 1
            )
            .1,
            22.0
        );
        store.apply_received_fields(&[(F::WindMph, 2.0)], now + 2, now + 2, true, "yard");
        let wind = store.current_condition_samples(now + 2)[1].clone().unwrap();
        assert_eq!(wind.source_id, "yard");
        assert_eq!(
            wind.selection_reason,
            "remote-station report expired; using fallback"
        );
    }

    #[test]
    fn selected_modeled_current_wind_stays_estimated_and_missing_is_not_calm() {
        let store = LiveWeatherStore::new();
        let now = 1_700_000_100;
        store.set_field_chains(HashMap::from([("wind_avg_mph", vec!["model".into()])]));
        store.apply_received_fields(
            &[(F::AirTempF, 80.0), (F::RhPct, 60.0), (F::WindMph, 1.0)],
            now,
            now,
            true,
            "yard",
        );
        store.apply_received_fields(&[(F::WindMph, 8.3)], now, now, false, "model");
        let samples = store.current_condition_samples(now);
        assert!(!samples[1].as_ref().unwrap().measured);
        assert!(samples[1]
            .as_ref()
            .unwrap()
            .summary_at(now)
            .contains("estimated"));
        assert_eq!(
            crate::assembly::readings::resolve_current_conditions(&samples, None, now),
            (80.0, 8.3, 60.0, LiveReadings::ForecastFallback)
        );
        assert_eq!(
            crate::assembly::readings::resolve_current_conditions(&samples, None, now + 601).3,
            LiveReadings::Unavailable
        );
    }
}
