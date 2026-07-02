// Shared store of the latest Tempest readings + a watch channel that the
// SSE endpoint subscribes to so browsers see updates the moment a packet
// lands. arc-swap gives us a copy-on-write Arc<Snapshot> so handlers can
// read the current state without taking a lock.

use crate::tempest::packets::StrikeEvent;
use serde::{Deserialize, Serialize};

#[cfg(feature = "ssr")]
use {
    crate::tempest::packets::{ObsSt, RapidWindOb},
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
    pub pop_pct: f64,
    /// Leaf wetness (%) from a leaf-wetness sensor (Davis WLL soil/leaf,
    /// Ecowitt WH35). Display + history only; 0.0 = unknown / not reported.
    #[serde(default)]
    pub leaf_wetness_pct: f64,
    pub precip_type: u8, // 0=none 1=rain 2=hail
    pub lightning_count_last_min: u32,
    pub lightning_strikes_last_hour: u32,
    pub lightning_recent: Vec<StrikeEvent>,
    pub lightning_avg_dist_mi: f64,
    pub last_strike_distance_mi: Option<f64>,
    pub last_strike_epoch: Option<i64>,
    pub battery_v: f64,
    pub battery_pct: f64,
    pub station_serial: String,
    pub hub_serial: String,
    /// Display name of the source currently driving these current-conditions
    /// (e.g. "Tempest", "Ecowitt", the source's config id, or "Demo"). Empty
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
    /// The irrigation engine's resolve_current_conditions consults these PER
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
    /// source keeps last_packet_epoch fresh. (Regression guard, see fix #5.)
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

/// How long a live station's last reading is considered "fresh" when a source
/// has no configured `max_age_s`. While a source has stamped a field within this
/// window, a same-or-lower contender (and a forecast fill) won't overwrite it
/// (see apply_source_fields). Tempest packets land ~every minute, so 10 minutes
/// tolerates a few missed packets without letting another source take over.
///
/// This is the FALLBACK only: every freshness check now consults the writing
/// source's PER-SOURCE `max_age` (see `TempestStore::max_age_for`) so a slow
/// cloud source (Open-Meteo / NWS / Met.no refresh every ~30 min) is not judged
/// stale at 10 min and demoted out from under a user pin (the owner's wind bug:
/// a 1800s-cadence pinned cloud lost wind to a 60s Tempest at the 600s mark).
#[cfg(feature = "ssr")]
const LIVE_FRESHNESS_SECS: i64 = 600;

/// The writer LABEL the Tempest UDP path stamps on the snapshot + the per-field
/// owners map. The single source of truth for that literal: `apply_obs` /
/// `apply_rapid_wind` write it, and `runtime::source_priority_map` /
/// `field_override_map` / `source_max_age_map` key the TempestUdp kind on this
/// same constant, so "the label a writer carries == the key the config maps use"
/// is one invariant rather than a string repeated across two files.
#[cfg(feature = "ssr")]
pub const TEMPEST_LABEL: &str = "Tempest";

/// The writer LABEL the NOAA MRMS adapter stamps on the snapshot owners maps: the
/// source's stable config id, the same id the region auto-seeder assigns
/// (`region::region_keyless_authority_entries`) and that `cloud_fill_field`
/// records as the fill owner. `max_age_for_field` keys the per-field rain-RATE
/// freshness override on this literal so the tight `MAX_AGE_MRMS_RATE_S` window
/// applies to the MRMS PrecipRate field and to nothing else.
#[cfg(feature = "ssr")]
pub const MRMS_WRITER_LABEL: &str = "noaa_mrms";

/// Priority assumed for a source the priorities map doesn't list (matches the
/// historical config default). Keeps single-source + test setups working.
#[cfg(feature = "ssr")]
const DEFAULT_SOURCE_PRIORITY: i32 = 50;

/// Per-field current-conditions ownership: maps a `WeatherField` to the stable
/// snapshot-field key the arbiter tracks ownership under. `None` for fields that
/// don't ride this scalar snapshot (string/forecast variants). The keys are the
/// snapshot struct field names so a reader can correlate.
#[cfg(feature = "ssr")]
fn field_owner_key(f: crate::ports::weather_source::WeatherField) -> Option<&'static str> {
    use crate::ports::weather_source::WeatherField::*;
    Some(match f {
        AirTempF => "air_temp_f",
        DewPointF => "dew_point_f",
        RhPct => "rh_pct",
        WindMph => "wind_avg_mph",
        WindGustMph => "wind_gust_mph",
        WindBearingDeg => "wind_dir_deg",
        PressureInHg => "pressure_inhg",
        SolarWm2 => "solar_w_m2",
        UvIndex => "uv_index",
        Illuminance => "illuminance_lx",
        RainTodayIn => "rain_in_today",
        RainIntensityInHr => "rain_intensity_in_hr",
        LightningCount => "lightning_count_last_min",
        LightningDistanceMi => "lightning_avg_dist_mi",
        Et0Today => "et0_today",
        FlowGpm => "flow_gpm",
        FlowTotalGalToday => "flow_total_gal_today",
        Pop => "pop_pct",
        LeafWetness => "leaf_wetness_pct",
        RainTypeStr | ForecastDaily | ForecastHourly => return None,
    })
}

/// Public accessor for `field_owner_key`: the stable snapshot-field key the
/// per-field arbiter (and the user override map) tracks ownership under, for a
/// `WeatherField`. `None` for fields that don't ride the scalar snapshot
/// (string / structured-forecast variants). The override-install path in
/// main.rs uses this to key `set_field_overrides` the same way the arbiter does.
#[cfg(feature = "ssr")]
pub fn override_owner_key(f: crate::ports::weather_source::WeatherField) -> Option<&'static str> {
    field_owner_key(f)
}

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

/// Whether the owner recorded at `(owner_epoch, owner_label)` is STALE as of
/// `at`, measured against the OWNER's configured `max_age` (per-source, falling
/// back to `LIVE_FRESHNESS_SECS`). Centralizes the freshness comparison so every
/// arbitration path (live claim, forecast fill, cloud fill, override decision)
/// judges a source by ITS OWN cadence instead of the one hardcoded 600s window.
#[cfg(feature = "ssr")]
fn owner_is_stale(max_age: i64, owner_epoch: i64, at: i64) -> bool {
    at.saturating_sub(owner_epoch) > max_age
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

#[cfg(feature = "ssr")]
pub struct TempestStore {
    current: ArcSwap<Snapshot>,
    tx: watch::Sender<Arc<Snapshot>>,
    rx: watch::Receiver<Arc<Snapshot>>,
    rolling: Mutex<RollingBuffers>,
    /// Per-source current-conditions priority, keyed by the label each writer
    /// uses ("Tempest" for the UDP path, the bus source_id otherwise). Set once
    /// at startup from config so the arbiter can rank live sources. Lock-free
    /// reads on the hot per-packet path.
    priorities: ArcSwap<HashMap<String, i32>>,
    /// Per-source MAX-AGE (seconds), keyed by the same writer label as
    /// `priorities` (`TEMPEST_LABEL` for the UDP path, the bus source_id
    /// otherwise). The freshness window the arbiter judges a source's OWNED
    /// fields by: a source is "still fresh" for `max_age` seconds after its last
    /// write. Set from `config.sources[*].max_age_s` at boot + on hot-reload, so
    /// a slow cloud cadence (Open-Meteo / NWS / Met.no ~1800s) is honored instead
    /// of the one hardcoded 600s window. Lock-free reads on the hot path; an
    /// unlisted source (or one with no configured max_age) falls back to
    /// `LIVE_FRESHNESS_SECS` via `max_age_for`.
    max_ages: ArcSwap<HashMap<String, i32>>,
    /// PER-FIELD ownership: snapshot-field key -> (owning live source's priority,
    /// epoch it last wrote, owning source label). The arbiter consults this so
    /// each field comes from its highest-priority fresh source, and a partial
    /// source only owns the fields it actually provides. The label lets the
    /// established owner refresh its OWN field at equal priority (otherwise the
    /// strict-`>` rule would block a single source from updating its own reading).
    field_owners: Mutex<HashMap<&'static str, (i32, i64, String)>>,
    /// PER-FIELD CLOUD-FILL ownership, the cloud-only fallback chain (fix #3a):
    /// snapshot-field key -> (filling cloud's priority, epoch it last filled,
    /// cloud label). SEPARATE from `field_owners` and from the live tier: a
    /// forecast/cloud source competes here by priority so a higher-priority cloud
    /// wins the fill and a cloud that goes stale past its `max_age` DEMOTES to the
    /// next-highest cloud, instead of "last writer wins" (the prior staleness-only
    /// fill let any cloud overwrite the display). Never sets `has_live_station`,
    /// the live epochs, or `last_packet_epoch` (a cloud fill is not a station).
    fill_owners: Mutex<HashMap<&'static str, (i32, i64, String)>>,
    /// PER-FIELD provenance for display: snapshot-field key -> the human source
    /// label that last wrote it (a live claim or a forecast fill). Powers the
    /// "which source drives each reading" panel. Separate from field_owners (and
    /// locked independently, never nested) so the display map never perturbs the
    /// hot arbitration path.
    field_provenance: Mutex<HashMap<&'static str, String>>,
    /// PER-FIELD user overrides: snapshot-field key -> the writer LABEL the
    /// operator pinned to own that field ("Tempest" for the UDP path, else the
    /// source id), installed from `config.field_source_overrides`. Empty (the
    /// default) means no override -> the priority arbitration below is unchanged,
    /// so a deployment that never sets one merges byte-identically. Lock-free
    /// reads on the hot per-packet path (arc-swapped at install only).
    field_overrides: ArcSwap<HashMap<&'static str, String>>,
    /// Freshness witness for the override/chain decision: (snapshot-field key,
    /// writer LABEL) -> (the epoch that CHAIN-ENTRY source last wrote that field,
    /// whether that source is a LIVE source). The arbiter records both whenever a
    /// chain entry writes its own field, and reads them (per chain entry) to decide
    /// which entry is the FIRST currently-fresh owner (so a later or off-chain
    /// source is blocked) or whether the whole chain is stale. Keyed by (field,
    /// label) rather than field alone so a multi-entry chain witnesses each entry's
    /// freshness independently (a single pin is just a 1-entry chain, so it keys one
    /// pair and behaves byte-identically). The is-live flag drives the TIER LOCK
    /// (fix #4): when the fresh chain owner is a CLOUD that then goes stale, a live
    /// station must NOT reclaim the field while some cloud still fills it (it demotes
    /// through the cloud fill chain instead); when the chain owner is a LIVE station
    /// that goes stale, the field falls back to the live priority merge so it is
    /// never lost. Only touched for keys that have a chain/override, so a plain
    /// deployment never locks it.
    field_override_seen: Mutex<HashMap<(&'static str, String), (i64, bool)>>,
    /// PER-FIELD user PRIORITY CHAINS: snapshot-field key -> the ORDERED list of
    /// writer LABELs the operator wants to own that field, primary first
    /// ("Tempest" for the UDP path, else the source id), installed from
    /// `config.field_source_chains`. The ordered-failover generalization of
    /// `field_overrides`: the FIRST label in the list that is currently FRESH owns
    /// the field, and if it goes quiet the next takes over. Empty (the default)
    /// means no chain -> `field_overrides` (a single pin) is consulted instead, and
    /// if that is empty too the priority arbitration below is unchanged, so a
    /// deployment that never sets one merges byte-identically. A ONE-element chain
    /// behaves byte-for-byte like the equivalent single pin. Lock-free reads on the
    /// hot per-packet path (arc-swapped at install only).
    field_chains: ArcSwap<HashMap<&'static str, Vec<String>>>,
}

#[cfg(feature = "ssr")]
#[derive(Default)]
struct RollingBuffers {
    pressure: VecDeque<(i64, f64)>, // last 6h of pressure samples
    strikes: VecDeque<StrikeEvent>, // last hour of strikes
    rain_today: f64,                // sum of rain_mm_last_min, day-bucket
    rain_today_day: i32,            // current LOCAL calendar-day bucket (num_days_from_ce)
}

/// Local calendar-day ordinal for a UNIX epoch: num_days_from_ce of the
/// local date, so the rain-today bucket rolls over at LOCAL midnight.
/// The previous `(epoch / 86400)` bucket was a UTC day despite its
/// comment, which zeroed the accumulator mid-evening for any tz west of
/// UTC and split overnight storms across two "days". Falls back to the
/// integer UTC day when the timestamp doesn't resolve to a local time.
#[cfg(feature = "ssr")]
fn local_day_ordinal(epoch: i64) -> i32 {
    use chrono::{Datelike, TimeZone};
    match chrono::Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.date_naive().num_days_from_ce(),
        None => (epoch / 86400) as i32,
    }
}

#[cfg(feature = "ssr")]
impl Default for TempestStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "ssr")]
impl TempestStore {
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
            field_owners: Mutex::new(HashMap::new()),
            fill_owners: Mutex::new(HashMap::new()),
            field_provenance: Mutex::new(HashMap::new()),
            field_overrides: ArcSwap::from(Arc::new(HashMap::new())),
            field_override_seen: Mutex::new(HashMap::new()),
            field_chains: ArcSwap::from(Arc::new(HashMap::new())),
        }
    }

    /// Per-field current-conditions provenance keyed by the canonical
    /// WeatherField name (e.g. `wind_mph -> "Tempest"`), for the snapshot's
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

    /// The current owner of the CURRENT-RAIN field (`rain_intensity_in_hr`) as of
    /// `at`, for the refresher's 3-tier honest rain gate. Prefers the LIVE tier:
    /// when a live station owns the rain rate freshly (judged by its own
    /// `max_age`), returns it with `is_live=true` (observation-grade `Measured`
    /// rain). Otherwise reports the CLOUD-FILL owner (the source whose forecast /
    /// observation / radar fill currently drives `rain_intensity_in_hr`), with
    /// `is_live=false` and its own freshness, so the refresher can map a fresh
    /// NWS/MRMS cloud owner to a measured/radar rain nature and surface that rate
    /// ABOVE the model forecast fallback. `None` when no source has written rain.
    #[cfg(feature = "ssr")]
    pub fn rain_owner(&self, at: i64) -> Option<RainOwner> {
        const RAIN_KEY: &str = "rain_intensity_in_hr";
        // LIVE tier first: a fresh live station owns the rain rate (a real gauge,
        // always observation-grade). A stale live owner yields to the cloud tier.
        {
            let owners = self.field_owners.lock().unwrap();
            if let Some((_, oe, ol)) = owners.get(RAIN_KEY) {
                if !owner_is_stale(self.max_age_for_field(ol, RAIN_KEY), *oe, at) {
                    return Some(RainOwner {
                        label: ol.clone(),
                        is_live: true,
                        is_fresh: true,
                    });
                }
            }
        }
        // CLOUD-FILL tier: the source currently filling rain for display. Report
        // it (with its freshness) so the refresher can surface a fresh NWS/MRMS
        // observation/radar rate above the model forecast. The rate field uses the
        // per-field freshness window (tight for the MRMS PrecipRate rate, so a
        // silent MRMS reports is_fresh=false in ~15 min instead of up to 2 hr).
        let fills = self.fill_owners.lock().unwrap();
        fills.get(RAIN_KEY).map(|(_, oe, ol)| RainOwner {
            label: ol.clone(),
            is_live: false,
            is_fresh: !owner_is_stale(self.max_age_for_field(ol, RAIN_KEY), *oe, at),
        })
    }

    /// Install the per-source current-conditions priority map (startup). Keys are
    /// the labels writers use: "Tempest" for the UDP path, the bus source_id
    /// otherwise.
    pub fn set_priorities(&self, map: HashMap<String, i32>) {
        self.priorities.store(Arc::new(map));
    }

    /// Priority for a source label, falling back to the default for unlisted
    /// sources (single-source + test setups, where the map is empty).
    fn priority_for(&self, label: &str) -> i32 {
        self.priorities
            .load()
            .get(label)
            .copied()
            .unwrap_or(DEFAULT_SOURCE_PRIORITY)
    }

    /// Install the per-source MAX-AGE map (boot + hot-reload). Keys are the same
    /// writer labels as `set_priorities` (`TEMPEST_LABEL` for the UDP path, the
    /// bus source_id otherwise); values are seconds. Mirrors `set_priorities` so
    /// a hot-reload re-ranks freshness identically to a restart. An empty map (no
    /// source configured a `max_age_s`) keeps the `LIVE_FRESHNESS_SECS` fallback.
    pub fn set_max_ages(&self, map: HashMap<String, i32>) {
        self.max_ages.store(Arc::new(map));
    }

    /// Freshness window (seconds) for a source label: its configured `max_age`,
    /// or `LIVE_FRESHNESS_SECS` (600) when the source set none (or is unlisted, as
    /// in single-source + test setups). This is the per-source replacement for the
    /// one hardcoded 600s window: a 1800s-cadence cloud (Open-Meteo / NWS / Met.no,
    /// configured ~2100) stays "fresh" through its full refresh interval, fixing
    /// the owner's wind-pin demote at the 600s mark.
    fn max_age_for(&self, label: &str) -> i64 {
        self.max_ages
            .load()
            .get(label)
            .copied()
            .map(i64::from)
            .unwrap_or(LIVE_FRESHNESS_SECS)
    }

    /// PER-FIELD freshness window for a (label, field) pair. Identical to
    /// `max_age_for` EXCEPT for the one field that needs a tighter window than its
    /// source's wide source-level window: the MRMS instantaneous PrecipRate RATE
    /// (`rain_intensity_in_hr`). MRMS reads TWO products per cycle into the same
    /// source label (`noaa_mrms`): the rate (valid ~now, ~15 min real cadence) and
    /// a gauge-corrected hourly accumulation (inherently ~1 to 1.5 hr late). The
    /// source-level `max_age` is deliberately wide (`MAX_AGE_MRMS_S`, 7200s) so the
    /// lagged accumulation field stays usable; routing the RATE through that wide
    /// window let a no-coverage coastal MRMS freeze the rain owner for up to 2 hr.
    /// So for the rate field specifically (and ONLY when the writer is the MRMS
    /// label) this returns the tight `MAX_AGE_MRMS_RATE_S` (900s); every other
    /// (label, field) delegates to `max_age_for`, so the accumulation field
    /// (`rain_in_today`) keeps the wide window. Net effect: a silent MRMS rate goes
    /// stale in ~15 min and Open-Meteo (model) takes the rain fill within minutes,
    /// while the gauge-corrected accumulation keeps its deliberately wide window.
    #[cfg(feature = "ssr")]
    fn max_age_for_field(&self, label: &str, field_key: &str) -> i64 {
        if field_key == "rain_intensity_in_hr" && label == MRMS_WRITER_LABEL {
            crate::config::region::MAX_AGE_MRMS_RATE_S as i64
        } else {
            self.max_age_for(label)
        }
    }

    /// PER-FIELD live arbitration: whether a LIVE source (priority `p`, epoch
    /// `at`, label `label`) may own field `key` now, recording ownership when it
    /// does. A source claims a field if no one owns it, IT is already the owner
    /// (refreshing its own reading), its priority is STRICTLY higher than the
    /// current owner's, or the owner went stale past the OWNER's `max_age`. A
    /// SINGLE live source owns every field it provides (nothing competes), so
    /// single-station setups are unchanged. A partial source (e.g. a soil gateway
    /// with only a barometer) claims ONLY the fields it actually provides, it can
    /// never zero out temp/RH/wind that another live station owns.
    #[cfg(feature = "ssr")]
    fn live_claim_field(
        &self,
        owners: &mut std::collections::HashMap<&'static str, (i32, i64, String)>,
        key: &'static str,
        p: i32,
        at: i64,
        label: &str,
    ) -> bool {
        let claim = match owners.get(key) {
            None => true,
            // The established owner always wins its OWN refresh, regardless of the
            // strict-`>` rule below (otherwise a single source could never update a
            // field it already owns at equal priority).
            Some((_, _, ol)) if ol == label => true,
            // Strict `>` (matches the forecast bridge): a STRICTLY-higher live
            // source wins, and a stale owner is yielded, but two EQUAL-priority
            // live sources do not flip-flop ownership of a shared field every tick.
            // The established owner keeps the field while fresh (judged by ITS OWN
            // max_age); a different equal-or-lower contender only takes over once
            // the owner goes stale.
            Some((op, oe, ol)) => p > *op || owner_is_stale(self.max_age_for(ol), *oe, at),
        };
        if claim {
            owners.insert(key, (p, at, label.to_string()));
        }
        claim
    }

    /// A forecast (non-live) source may FILL field `key` (for display) only when
    /// no LIVE source owns it freshly (judged by the live owner's `max_age`). It
    /// never records live ownership, so a live source always reclaims. The CLOUD
    /// chain among forecast sources is arbitrated separately by `cloud_fill_field`
    /// (priority-aware demote); this gate only protects a live station from a fill.
    #[cfg(feature = "ssr")]
    fn forecast_may_fill(
        &self,
        owners: &std::collections::HashMap<&'static str, (i32, i64, String)>,
        key: &'static str,
        at: i64,
    ) -> bool {
        match owners.get(key) {
            None => true,
            Some((_, oe, ol)) => owner_is_stale(self.max_age_for(ol), *oe, at),
        }
    }

    /// Whether the CLOUD-FILL tier for field `key` is still FRESH as of `at`,
    /// judged by the current fill owner's own `max_age`. Reads a borrowed `fills`
    /// map (no locking, so the caller controls lock order). Drives the
    /// last-resort backup change to the tier lock (below): a pinned CLOUD that has
    /// gone stale still blocks a live station from reclaiming the field ONLY while
    /// SOME cloud is still actively filling it; once the whole cloud tier for the
    /// field is stale or exhausted (no fill owner, or its window elapsed), the
    /// block lifts so the wind-shadowed Tempest can take the field as the last
    /// resort instead of leaving it pinned to a dead cloud. The pin still sticks:
    /// the moment any cloud refills the field fresh, the lock re-engages and the
    /// cloud re-wins. The field is never blanked either way.
    #[cfg(feature = "ssr")]
    fn cloud_tier_fresh(
        &self,
        fills: &std::collections::HashMap<&'static str, (i32, i64, String)>,
        key: &'static str,
        at: i64,
    ) -> bool {
        match fills.get(key) {
            // No cloud has ever filled this field, or none currently owns the
            // fill: the cloud tier is exhausted, so the lock must not hold.
            None => false,
            Some((_, oe, ol)) => !owner_is_stale(self.max_age_for(ol), *oe, at),
        }
    }

    /// CLOUD-FILL arbitration (fix #3a, priority-aware fallback chain). Operates
    /// ONLY on `fill_owners` and NEVER on the live tier: a cloud source claims a
    /// field's FILL if no cloud owns it, IT already owns it (refresh), its priority
    /// is STRICTLY higher than the current cloud owner's, or that cloud owner went
    /// stale past ITS `max_age` (demote to the next-highest cloud). Mirrors
    /// `live_claim_field` but records nothing in `field_owners` and sets no live
    /// epoch / `has_live_station` / `last_packet_epoch`, so a cloud fill never
    /// reads as a live station. The caller (the `live_current == false` branch of
    /// `apply_source_fields`) still gates the actual snapshot write through
    /// `forecast_may_fill` so a fresh LIVE station is never overwritten by a fill.
    #[cfg(feature = "ssr")]
    fn cloud_fill_field(
        &self,
        fills: &mut std::collections::HashMap<&'static str, (i32, i64, String)>,
        key: &'static str,
        p: i32,
        at: i64,
        label: &str,
    ) -> bool {
        let claim = match fills.get(key) {
            None => true,
            Some((_, _, ol)) if ol == label => true,
            // The current owner's staleness is judged by its PER-FIELD window, so
            // a silent MRMS PrecipRate rate (`rain_intensity_in_hr`) demotes to the
            // next cloud (Open-Meteo) in ~15 min instead of inheriting MRMS's wide
            // 2 hr accumulation window. Every other field delegates to the
            // source-level window unchanged.
            Some((op, oe, ol)) => {
                p > *op || owner_is_stale(self.max_age_for_field(ol, key), *oe, at)
            }
        };
        if claim {
            fills.insert(key, (p, at, label.to_string()));
        }
        claim
    }

    /// Install the per-field user overrides (startup). Keys are the same
    /// snapshot-field keys the arbiter tracks ownership under (see
    /// `field_owner_key`); values are the writer LABEL pinned to own that field
    /// ("Tempest" for the UDP path, the bus source id otherwise) so the override
    /// compares directly against the label each writer carries. An empty map
    /// disables overrides entirely (the priority merge is unchanged).
    pub fn set_field_overrides(&self, map: HashMap<&'static str, String>) {
        self.field_overrides.store(Arc::new(map));
    }

    /// Install the per-field user PRIORITY CHAINS (boot + hot-reload). Keys are
    /// the same snapshot-field keys the arbiter tracks ownership under (see
    /// `field_owner_key`); values are the ORDERED list of writer LABELs, primary
    /// first ("Tempest" for the UDP path, the bus source id otherwise), so the
    /// chain compares directly against the label each writer carries. An empty map
    /// disables chains (the single-pin `field_overrides` and then the priority
    /// merge apply unchanged). Mirrors `set_field_overrides` so a hot-reload
    /// re-chains fields identically to a restart.
    pub fn set_field_chains(&self, map: HashMap<&'static str, Vec<String>>) {
        self.field_chains.store(Arc::new(map));
    }

    /// Resolve the effective ORDERED chain of owner LABELs for field `key`: the
    /// installed `field_source_chains` entry when present, else the legacy single
    /// `field_source_overrides` pin treated as a one-element chain, else `None`
    /// (no chain and no pin -> the unchanged priority merge governs the field).
    /// This is the single point that keeps BOTH mechanisms working: a chain and a
    /// lone pin never coexist for the same field (the chain wins if both are set),
    /// and a one-element chain is byte-for-byte the old pin. Returns an owned
    /// `Vec` (a clone off the arc-swapped maps) so the hot path never holds either
    /// arc load across the freshness lock.
    #[cfg(feature = "ssr")]
    fn field_chain_for(&self, key: &str) -> Option<Vec<String>> {
        if let Some(chain) = self.field_chains.load().get(key) {
            // field_chain_map already dropped dead entries + empty chains, so a
            // present chain is non-empty; guard anyway so an empty install can
            // never blank the field (falls through to the pin/priority merge).
            if !chain.is_empty() {
                return Some(chain.clone());
            }
        }
        self.field_overrides
            .load()
            .get(key)
            .map(|want| vec![want.clone()])
    }

    /// PER-FIELD override arbitration. Consulted BEFORE the priority claim for a
    /// field `key` that a writer (label `label`, epoch `at`, `writer_is_live`) is
    /// trying to set. Returns:
    ///   * `Some(true)`  -> force the claim (this writer IS the field's current
    ///                      chain owner: the FIRST chain entry that is fresh).
    ///   * `Some(false)` -> block the claim (a DIFFERENT writer must not take the
    ///                      field): a fresh EARLIER chain entry still owns it, OR
    ///                      the TIER LOCK below blocks a live station from
    ///                      reclaiming a stale-chain CLOUD field.
    ///   * `None`        -> no chain/override on this field, OR the ENTIRE chain is
    ///                      stale/never-seen and the writer is allowed to fall
    ///                      through to the normal priority merge for this field (a
    ///                      stale LIVE-station chain yields to the live priority
    ///                      merge; a stale CLOUD chain yields to the cloud fill
    ///                      chain, which the cloud-write path runs after `None`).
    ///
    /// THE CHAIN GENERALIZATION. The field's effective chain is
    /// `field_source_chains` (an ordered list of writer labels) when set, else the
    /// legacy single `field_source_overrides` pin as a ONE-element chain, else
    /// nothing (`field_chain_for`). The writing `label` OWNS the field iff it is
    /// the FIRST source in the chain that is currently FRESH: a writer NOT in the
    /// chain, or LATER in the chain than a still-fresh earlier entry, is blocked.
    /// When the primary goes quiet the next fresh entry takes over (ordered
    /// failover), and when the primary recovers it reclaims (it is earlier). A
    /// ONE-element chain reduces byte-for-byte to the old single pin.
    ///
    /// FRESHNESS is judged by EACH chain entry's own `max_age` (fix #2): a
    /// 1800s-cadence cloud entry (configured ~2100) keeps the field well past 600s,
    /// fixing the owner's wind-pin demote. Every chain entry that writes stamps its
    /// own `(field, label)` freshness + tier in `field_override_seen`, so the
    /// "first fresh entry" is decided against each entry's real last-write epoch.
    ///
    /// NEVER-BLANK INVARIANT (fix #4, with last-resort backup): when the WHOLE
    /// chain is stale or absent this returns `None`, so the field falls through to
    /// the existing priority merge and a non-chain source can still win it: a
    /// reading is NEVER blanked. The TIER LOCK adds ONE exception for the honest
    /// live-vs-cloud tier: if the most-recently-seen chain owner was a CLOUD that
    /// has gone stale, a LIVE writer is blocked (`Some(false)`) ONLY WHILE some
    /// cloud is still fresh for this field (`cloud_tier_fresh`); once the whole
    /// cloud tier is stale or exhausted the block lifts and the live station
    /// reclaims via its priority merge (`None`). A CLOUD writer always falls
    /// through (`None`) to demote down the cloud fill chain, and a stale LIVE-station
    /// chain yields to the live priority merge (`None`), so an offline chain never
    /// costs the field a reading.
    ///
    /// A plain deployment (no chains, no overrides) short-circuits on the lock-free
    /// `field_chains`/`field_overrides` reads and never touches the witness map, so
    /// it is byte-identical to before.
    ///
    /// `cloud_tier_fresh` is precomputed by the caller (which holds the `fills`
    /// lock, if any) and passed in so this method never locks `fill_owners`,
    /// keeping the owners -> fills lock order in `apply_source_fields` intact.
    #[cfg(feature = "ssr")]
    fn override_decision(
        &self,
        key: &'static str,
        label: &str,
        at: i64,
        writer_is_live: bool,
        cloud_tier_fresh: bool,
    ) -> Option<bool> {
        // The effective chain: the ordered chain if set, else the single pin as a
        // 1-element chain, else nothing -> defer to the unchanged priority merge.
        let chain = self.field_chain_for(key)?;

        let mut seen = self.field_override_seen.lock().unwrap();
        // If THIS writer is a chain entry, stamp its freshness + tier first, so the
        // "first fresh entry" scan below sees this write (a recovering primary
        // reclaims the moment it writes; a standby entry becomes eligible to take
        // over once an earlier entry goes stale).
        let writer_in_chain = chain.iter().any(|c| c == label);
        if writer_in_chain {
            seen.insert((key, label.to_string()), (at, writer_is_live));
        }
        // Walk the chain in order: the FIRST entry that is currently FRESH (was
        // witnessed AND is not stale past ITS OWN max_age) is the owner. Also track
        // the MOST-RECENTLY-SEEN entry (max epoch), whose live/cloud tier drives the
        // last-resort tier lock when the whole chain is stale.
        let mut owner: Option<&String> = None;
        let mut last_seen: Option<(i64, bool)> = None;
        for entry in &chain {
            if let Some(&(epoch, is_live)) = seen.get(&(key, entry.clone())) {
                match last_seen {
                    Some((best, _)) if best >= epoch => {}
                    _ => last_seen = Some((epoch, is_live)),
                }
                // Per-FIELD freshness window (not the source-level one): this is
                // what gives MRMS rain_intensity_in_hr its tight ~900s window, so a
                // silent radar-rate source yields to the next chain entry instead of
                // holding the field for the wide ~2h source window.
                if owner.is_none() && !owner_is_stale(self.max_age_for_field(entry, key), epoch, at)
                {
                    owner = Some(entry);
                }
            }
        }
        if let Some(owner_label) = owner {
            // A fresh chain entry owns the field. The owner itself writing wins;
            // any OTHER writer (a later-in-chain entry, or an off-chain source) is
            // blocked while the earlier entry stays fresh.
            return Some(owner_label == label);
        }
        // No fresh chain entry. Whole chain stale or never-seen.
        let Some((_, owner_is_live)) = last_seen else {
            // No chain entry has ever written: defer to the normal merge so the
            // chain never blanks a field whose owners have not yet reported.
            return None;
        };
        drop(seen);
        // TIER LOCK (last-resort backup): the most-recently-seen chain owner was a
        // CLOUD (owner_is_live=false) that is now stale. A LIVE writer is blocked
        // from reclaiming ONLY WHILE some cloud is still fresh for this field; once
        // the whole cloud tier is stale or exhausted (cloud_tier_fresh is false),
        // the block lifts and the live station reclaims via its normal priority
        // merge (None), so a wind-shadowed Tempest is the LAST RESORT rather than
        // the field staying stuck on a dead cloud chain. A CLOUD writer always
        // falls through (None) to demote down the cloud fill chain; a stale
        // LIVE-station chain yields to the live priority merge (None). The field is
        // NEVER blanked either way.
        if !owner_is_live && writer_is_live && cloud_tier_fresh {
            Some(false)
        } else {
            None
        }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.rx.clone()
    }

    /// Replace the snapshot wholesale. Used by demo-mode synthesis to
    /// drop synthetic data into the live store without going through
    /// the per-packet apply_* path. Real packet processing should
    /// continue to use apply_obs / apply_rapid_wind / apply_strike so
    /// rolling buffers stay accurate.
    pub fn store(&self, snap: Snapshot) {
        let arc = Arc::new(snap);
        self.current.store(arc.clone());
        let _ = self.tx.send(arc);
    }

    /// Seed the rain-today accumulator from persisted history after a
    /// restart, so a mid-storm reboot doesn't zero the daily total (the
    /// per-minute UDP packets carry deltas, not the accumulation). No-op
    /// unless `day_epoch` falls on the current local day. Only ever
    /// raises the accumulator: packets that landed since boot stay
    /// counted when they already exceed the seed. Returns true when the
    /// seed was applied.
    pub fn seed_rain_today(&self, rain_in: f64, day_epoch: i64) -> bool {
        let bucket = local_day_ordinal(day_epoch);
        let today = local_day_ordinal(chrono::Utc::now().timestamp());
        if bucket != today || rain_in <= 0.0 {
            return false;
        }
        let rain_mm = rain_in * 25.4;
        let mut roll = self.rolling.lock().unwrap();
        if roll.rain_today_day == bucket {
            if roll.rain_today >= rain_mm {
                return false;
            }
            roll.rain_today = rain_mm;
        } else {
            // A stale (or default-zero) bucket never carries into today.
            roll.rain_today_day = bucket;
            roll.rain_today = rain_mm;
        }
        true
    }

    pub fn apply_obs(&self, station_serial: &str, hub_serial: &str, obs: &ObsSt) {
        let mut roll = self.rolling.lock().unwrap();

        // Trim pressure buffer to last 6 hours.
        let now = obs.time_epoch;
        let six_hours_ago = now - 6 * 3600;
        while roll
            .pressure
            .front()
            .is_some_and(|(t, _)| *t < six_hours_ago)
        {
            roll.pressure.pop_front();
        }
        let pressure_inhg = obs.pressure_mb * 0.02953;
        roll.pressure.push_back((now, pressure_inhg));

        // Today rain accumulation, bucketed by the LOCAL calendar day of
        // the observation so the total resets at local midnight.
        let day_bucket = local_day_ordinal(now);
        if roll.rain_today_day != day_bucket {
            roll.rain_today_day = day_bucket;
            roll.rain_today = 0.0;
        }
        roll.rain_today += obs.rain_mm_last_min;
        let rain_today_in = roll.rain_today / 25.4;

        // Trim strike buffer to last hour.
        let one_hour_ago = now - 3600;
        while roll
            .strikes
            .front()
            .is_some_and(|s| s.time_epoch < one_hour_ago)
        {
            roll.strikes.pop_front();
        }
        let last = roll.strikes.back().cloned();
        let pressure_trend: Vec<_> = roll.pressure.iter().cloned().collect();
        drop(roll);

        let air_temp_f = obs.air_temp_c * 9.0 / 5.0 + 32.0;
        let dew_c = dew_point_c(obs.air_temp_c, obs.rh_pct);
        let dew_f = dew_c * 9.0 / 5.0 + 32.0;
        let wet_c = wet_bulb_c(obs.air_temp_c, obs.rh_pct);
        let wet_f = wet_c * 9.0 / 5.0 + 32.0;
        let wind_avg_mph = obs.wind_avg_mps * 2.23694;
        let feels_f = feels_like_f(air_temp_f, obs.rh_pct, wind_avg_mph);

        let prev = self.current.load_full();
        let prio = self.priority_for(TEMPEST_LABEL);
        let mut new = Snapshot {
            last_packet_epoch: now,
            air_temp_f,
            feels_like_f: feels_f,
            dew_point_f: dew_f,
            wet_bulb_f: wet_f,
            rh_pct: obs.rh_pct,
            pressure_inhg,
            pressure_trend_inhg: pressure_trend,
            wind_lull_mph: obs.wind_lull_mps * 2.23694,
            wind_avg_mph,
            wind_gust_mph: obs.wind_gust_mps * 2.23694,
            wind_dir_deg: obs.wind_dir_deg,
            // rapid_wind keeps its own values; carry forward whatever the last
            // 3s tick set so a fresh obs_st doesn't blank them out.
            rapid_wind_mph: prev.rapid_wind_mph,
            rapid_wind_dir: prev.rapid_wind_dir,
            illuminance_lx: obs.illuminance_lx,
            uv_index: obs.uv_index,
            solar_w_m2: obs.solar_w_m2,
            rain_in_last_min: obs.rain_mm_last_min / 25.4,
            rain_in_today: rain_today_in,
            // 60 * (mm/min) → mm/hr → in/hr.
            rain_intensity_in_hr: obs.rain_mm_last_min * 60.0 / 25.4,
            // ET0 / flow / POP are not in the Tempest packet; carry forward
            // whatever a bus source last contributed so an obs_st doesn't blank
            // them out (mirrors how rapid_wind / lightning are carried).
            et0_today: prev.et0_today,
            flow_gpm: prev.flow_gpm,
            flow_total_gal_today: prev.flow_total_gal_today,
            pop_pct: prev.pop_pct,
            leaf_wetness_pct: prev.leaf_wetness_pct,
            precip_type: obs.precip_type,
            lightning_count_last_min: obs.lightning_strike_count_last_min,
            lightning_strikes_last_hour: prev.lightning_strikes_last_hour,
            lightning_recent: prev.lightning_recent.clone(),
            lightning_avg_dist_mi: obs.lightning_avg_dist_km * 0.621371,
            last_strike_distance_mi: last.as_ref().map(|s| s.distance_km * 0.621371),
            last_strike_epoch: last.as_ref().map(|s| s.time_epoch),
            battery_v: obs.battery_v,
            battery_pct: Snapshot::battery_pct_from_v(obs.battery_v),
            station_serial: station_serial.to_string(),
            hub_serial: hub_serial.to_string(),
            source_label: TEMPEST_LABEL.to_string(),
            owner_priority: prio,
            // Tempest is a live local station: by definition a station is
            // present and producing this packet. Stays true on every Tempest
            // write regardless of which fields a higher-priority source owns.
            has_live_station: true,
            // Tempest is a live station; these are set to `now` for the fields it
            // owns and reverted to prev below for any a higher source owns.
            air_temp_live_epoch: now,
            wind_live_epoch: now,
            rh_live_epoch: now,
            // Set to `now` only if Tempest actually owns rain_intensity this
            // packet (resolved below); else carry the prior live-rain epoch so a
            // forecast-filled rain reading never reads as a live station rate.
            rain_live_epoch: prev.rain_live_epoch,
        };
        // PER-FIELD arbitration: Tempest claims each weather field it measures;
        // if a strictly-higher-priority live source already owns one, keep that
        // source's value (don't overwrite). A single station (or the highest)
        // claims everything, so a Tempest-only setup is unchanged. The rolling
        // buffers above always update, so they stay warm for when Tempest
        // reclaims. Tempest-exclusive fields (rapid wind, lightning ring,
        // battery, serial, pressure trend) are never contested.
        // Fields Tempest wins this packet -> recorded as "Tempest" provenance
        // after the owners lock drops (reverted fields keep the other source's).
        let mut prov_owned: Vec<&'static str> = Vec::new();
        let (tempest_owns_air, tempest_owns_wind, tempest_owns_rh, tempest_owns_rain) = {
            let mut owners = self.field_owners.lock().unwrap();
            // Read-only handle on the cloud-fill tier for the last-resort backup
            // signal the override tier lock needs. Locked right after `owners`
            // (the same owners -> fills order apply_source_fields uses) so the two
            // paths can never deadlock. Tempest never WRITES the fill tier, so this
            // is a read-only borrow used purely for the cloud_tier_fresh check.
            let fills = self.fill_owners.lock().unwrap();
            // PER-FIELD claim, override-aware: the user override (writer label
            // "Tempest" for this path) precedes priority. Some(true) forces the
            // claim, Some(false) blocks it (a pinned-elsewhere field stays with
            // its owner), None defers to the unchanged priority arbitration.
            macro_rules! claim {
                ($key:literal) => {
                    // Tempest UDP is a LIVE writer; its label is the single-source
                    // TEMPEST_LABEL constant the config maps key it under. The tier
                    // lock blocks Tempest from a cloud-pinned field ONLY while the
                    // cloud tier is still fresh for that field (last-resort backup).
                    match self.override_decision(
                        $key,
                        TEMPEST_LABEL,
                        now,
                        true,
                        self.cloud_tier_fresh(&fills, $key, now),
                    ) {
                        Some(true) => {
                            owners.insert($key, (prio, now, TEMPEST_LABEL.to_string()));
                            true
                        }
                        Some(false) => false,
                        None => self.live_claim_field(&mut owners, $key, prio, now, TEMPEST_LABEL),
                    }
                };
            }
            macro_rules! contest {
                ($key:literal, $field:ident) => {
                    if claim!($key) {
                        prov_owned.push($key);
                    } else {
                        new.$field = prev.$field;
                    }
                };
            }
            // Engine-critical fields (air temp / wind / RH) capture ownership so
            // their per-field live epoch reflects whether Tempest actually owns them.
            let owns_air = claim!("air_temp_f");
            if owns_air {
                prov_owned.push("air_temp_f");
            } else {
                new.air_temp_f = prev.air_temp_f;
            }
            let owns_wind = claim!("wind_avg_mph");
            if owns_wind {
                prov_owned.push("wind_avg_mph");
            } else {
                new.wind_avg_mph = prev.wind_avg_mph;
            }
            let owns_rh = claim!("rh_pct");
            if owns_rh {
                prov_owned.push("rh_pct");
            } else {
                new.rh_pct = prev.rh_pct;
            }
            contest!("dew_point_f", dew_point_f);
            contest!("pressure_inhg", pressure_inhg);
            contest!("wind_gust_mph", wind_gust_mph);
            contest!("wind_dir_deg", wind_dir_deg);
            contest!("solar_w_m2", solar_w_m2);
            contest!("uv_index", uv_index);
            contest!("illuminance_lx", illuminance_lx);
            contest!("rain_in_today", rain_in_today);
            // Rain intensity captures ownership so its per-field LIVE epoch
            // reflects whether Tempest actually owns the current-rain rate (the
            // engine's "currently raining" path keys on rain_live_epoch).
            let owns_rain = claim!("rain_intensity_in_hr");
            if owns_rain {
                prov_owned.push("rain_intensity_in_hr");
            } else {
                new.rain_intensity_in_hr = prev.rain_intensity_in_hr;
            }
            contest!("lightning_count_last_min", lightning_count_last_min);
            contest!("lightning_avg_dist_mi", lightning_avg_dist_mi);
            (owns_air, owns_wind, owns_rh, owns_rain)
        };
        // Record display provenance for the fields Tempest owns (separate lock).
        {
            let mut prov = self.field_provenance.lock().unwrap();
            for key in &prov_owned {
                prov.insert(key, TEMPEST_LABEL.to_string());
            }
        }
        // Per-field live epochs: `now` for fields Tempest owns, else carry the
        // owning source's epoch (so the engine never treats Tempest as the live
        // source of a field a higher source owns).
        new.air_temp_live_epoch = if tempest_owns_air {
            now
        } else {
            prev.air_temp_live_epoch
        };
        new.wind_live_epoch = if tempest_owns_wind {
            now
        } else {
            prev.wind_live_epoch
        };
        new.rh_live_epoch = if tempest_owns_rh {
            now
        } else {
            prev.rh_live_epoch
        };
        // Live-rain epoch: `now` only when Tempest owns the rain rate this
        // packet, else carry the prior owner's epoch so the engine never treats
        // Tempest as the live rain source of a rate a higher source owns.
        new.rain_live_epoch = if tempest_owns_rain {
            now
        } else {
            prev.rain_live_epoch
        };
        // Recompute feels-like from the FINAL merged temp/rh/wind (any of which
        // may have reverted to a higher source's value).
        new.feels_like_f = feels_like_f(new.air_temp_f, new.rh_pct, new.wind_avg_mph);
        // Headline provenance: if a higher-priority source owns the air temp,
        // keep its label/priority + its temp-derived wet-bulb.
        if !tempest_owns_air {
            new.source_label = prev.source_label.clone();
            new.owner_priority = prev.owner_priority;
            new.wet_bulb_f = prev.wet_bulb_f;
        }
        let new = Arc::new(new);
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    pub fn apply_rapid_wind(&self, ob: &RapidWindOb) {
        let prev = self.current.load_full();
        // Rapid wind is Tempest-specific; only update the snapshot while Tempest
        // owns current conditions (or before any source has claimed), so it can't
        // leak Tempest's 3s wind over a higher-priority non-Tempest owner.
        if !prev.source_label.is_empty() && prev.source_label != TEMPEST_LABEL {
            return;
        }
        let new = Arc::new(Snapshot {
            rapid_wind_mph: ob.speed_mps * 2.23694,
            rapid_wind_dir: ob.direction_deg,
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    /// Apply a batch of merged-bus weather fields from a NON-Tempest source,
    /// carrying every other field forward (mirrors apply_rapid_wind). This is
    /// the bridge that lets Ecowitt / Open-Meteo / HA-passthrough / Davis / etc.
    /// populate the dashboard + HA weather entities, which all read this
    /// snapshot (previously only Tempest UDP/demo/Blitzortung wrote it).
    ///
    /// Arbitration is PER FIELD (see field_owner_key / live_claim_field):
    /// - A live source (live_current=true) claims a field if its priority >= the
    ///   field's current owner (or the owner is stale), sets the value, and
    ///   records ownership + the field's live epoch. It also stamps
    ///   `last_packet_epoch` (whole-snapshot freshness) for any field it writes.
    /// - A forecast source (live_current=false) FILLS a field only when no live
    ///   source owns it freshly, never records ownership, and never stamps
    ///   `last_packet_epoch` or the per-field live epochs -- so the engine's
    ///   resolve_current_conditions (which reads the per-field live epochs) never
    ///   treats forecast-filled data as a live station reading.
    pub fn apply_source_fields(
        &self,
        fields: &[(crate::ports::weather_source::WeatherField, f64)],
        at_epoch: i64,
        live_current: bool,
        source_label: &str,
    ) {
        use crate::ports::weather_source::WeatherField as F;
        let prev = self.current.load_full();
        let prio = self.priority_for(source_label);
        let mut snap = (*prev).clone();
        let mut owners = self.field_owners.lock().unwrap();
        // Cloud-fill chain (fix #3a). Locked right after `field_owners` so the two
        // locks always nest in the SAME order (owners -> fills) here, the only
        // place both are held (apply_obs touches only `field_owners`), so no
        // deadlock. A forecast/cloud write competes here by priority + max_age; a
        // live write never touches it.
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
        // Keys this source wrote this call -> recorded as provenance after the
        // owners lock drops (a live claim or a forecast fill both count).
        let mut prov_keys: Vec<&'static str> = Vec::new();
        for (field, value) in fields {
            let v = *value;
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
            let cloud_tier_fresh = self.cloud_tier_fresh(&fills, key, at_epoch);
            let allowed = match self.override_decision(
                key,
                source_label,
                at_epoch,
                live_current,
                cloud_tier_fresh,
            ) {
                Some(true) => {
                    // Force ownership so the field reads as live/owned and a
                    // later same-or-lower priority source can't steal it within
                    // the freshness window; mirrors a normal live claim. A pinned
                    // CLOUD records in the cloud chain instead (it is not a live
                    // owner) so its own demote-on-stale is tracked there.
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
                        self.live_claim_field(&mut owners, key, prio, at_epoch, source_label)
                    } else {
                        // CLOUD FILL (fix #3a, priority-aware): only when no fresh
                        // LIVE station owns the field (forecast_may_fill), AND this
                        // cloud wins the cloud chain over any fresher higher cloud
                        // (cloud_fill_field, which also demotes a cloud that went
                        // stale past its max_age to the next-highest). Both gates
                        // must pass: the live gate protects a real station, the
                        // cloud gate ranks the clouds among themselves.
                        self.forecast_may_fill(&owners, key, at_epoch)
                            && self.cloud_fill_field(&mut fills, key, prio, at_epoch, source_label)
                    }
                }
            };
            if !allowed {
                continue;
            }
            // A live source is writing a current-conditions field this call:
            // a real station is present and producing. A forecast fill
            // (live_current=false) writes for display but never sets this.
            if live_current {
                live_claimed = true;
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
                F::PressureInHg => snap.pressure_inhg = v,
                F::SolarWm2 => snap.solar_w_m2 = v,
                F::UvIndex => snap.uv_index = v,
                F::Illuminance => snap.illuminance_lx = v,
                F::RainTodayIn => snap.rain_in_today = v,
                F::RainIntensityInHr => {
                    snap.rain_intensity_in_hr = v;
                    owns_rain = true;
                }
                F::LightningCount => snap.lightning_count_last_min = v.max(0.0) as u32,
                F::LightningDistanceMi => snap.lightning_avg_dist_mi = v,
                F::Et0Today => snap.et0_today = v,
                F::FlowGpm => snap.flow_gpm = v,
                F::FlowTotalGalToday => snap.flow_total_gal_today = v,
                F::Pop => snap.pop_pct = v,
                F::LeafWetness => snap.leaf_wetness_pct = v,
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
        // Record display provenance (separate lock, never nested with owners).
        if !source_label.is_empty() {
            let mut prov = self.field_provenance.lock().unwrap();
            for key in prov_keys {
                prov.insert(key, source_label.to_string());
            }
        }
        // Recompute feels-like from the (possibly updated) temp/rh/wind.
        snap.feels_like_f = feels_like_f(snap.air_temp_f, snap.rh_pct, snap.wind_avg_mph);
        // Headline current-conditions provenance follows whoever owns the air
        // temperature; a source that only contributed (e.g.) pressure does NOT
        // hijack the headline label. A LIVE air-temp owner always sets the
        // headline. A FORECAST fill (live_current=false) only sets it when the
        // snapshot has no source yet (a cloud-only deployment with no station),
        // so it can't steal a real station's label for ~60s while the station is
        // momentarily not owning (the ~30-min forecast tick would otherwise win
        // the headline between station packets). (fix #4b)
        if owns_air_temp && !source_label.is_empty() {
            if live_current {
                snap.source_label = source_label.to_string();
                snap.owner_priority = prio;
            } else if snap.source_label.is_empty() {
                snap.source_label = source_label.to_string();
            }
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
            if owns_air_temp {
                snap.air_temp_live_epoch = at_epoch;
            }
            if owns_wind {
                snap.wind_live_epoch = at_epoch;
            }
            if owns_rh {
                snap.rh_live_epoch = at_epoch;
            }
            // Live-rain epoch advances ONLY on a LIVE rain write. Open-Meteo
            // current (live_current=false) fills rain_intensity for display but
            // must NOT mark it live, so a stale cloud rate never reads as live
            // station rain in the engine's "currently raining" gate.
            if owns_rain {
                snap.rain_live_epoch = at_epoch;
            }
        }
        let new = Arc::new(snap);
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }

    pub fn apply_strike(&self, evt: &StrikeEvent) {
        self.apply_strikes(std::slice::from_ref(evt));
    }

    /// Batch strike insert: one lock, one snapshot swap, one SSE event
    /// for the whole slice. The Tempest UDP path delegates here one
    /// strike at a time; the Blitzortung feed batches because the
    /// community network can deliver several strikes per second during
    /// an outbreak and each swap broadcasts a full snapshot.
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

    pub fn apply_battery(&self, voltage: f64) {
        let prev = self.current.load_full();
        let new = Arc::new(Snapshot {
            battery_v: voltage,
            battery_pct: Snapshot::battery_pct_from_v(voltage),
            ..(*prev).clone()
        });
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }
}

/// Magnus-Tetens dew point (°C) from temperature (°C) and RH (%).
#[cfg(feature = "ssr")]
fn dew_point_c(t_c: f64, rh: f64) -> f64 {
    let a = 17.625;
    let b = 243.04;
    let alpha = (rh.max(1.0) / 100.0).ln() + a * t_c / (b + t_c);
    b * alpha / (a - alpha)
}

/// Stull (2011) wet-bulb approximation, valid for normal RH/temp ranges.
#[cfg(feature = "ssr")]
fn wet_bulb_c(t_c: f64, rh: f64) -> f64 {
    let rh = rh.max(1.0);
    t_c * (0.151_977 * (rh + 8.313_659).sqrt()).atan() + (t_c + rh).atan() - (rh - 1.676_331).atan()
        + 0.003_918_38 * rh.powf(1.5) * (0.023_101 * rh).atan()
        - 4.686_035
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

    fn obs(epoch: i64, rain_mm: f64) -> ObsSt {
        ObsSt {
            time_epoch: epoch,
            rain_mm_last_min: rain_mm,
            ..Default::default()
        }
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
        let store = TempestStore::new();
        // 23:30 local: 5.08 mm = 0.20".
        store.apply_obs("ST", "HB", &obs(local_epoch(2026, 1, 15, 23, 30), 5.08));
        assert!((store.snapshot().rain_in_today - 0.20).abs() < 1e-9);
        // 23:50 same local day: accumulates to 0.30".
        store.apply_obs("ST", "HB", &obs(local_epoch(2026, 1, 15, 23, 50), 2.54));
        assert!((store.snapshot().rain_in_today - 0.30).abs() < 1e-9);
        // 00:30 the NEXT local day: bucket rolls, total restarts at 0.10".
        // With the old UTC bucketing this either kept accumulating or had
        // already reset hours before local midnight, tz-dependent.
        store.apply_obs("ST", "HB", &obs(local_epoch(2026, 1, 16, 0, 30), 2.54));
        assert!((store.snapshot().rain_in_today - 0.10).abs() < 1e-9);
    }

    #[test]
    fn seed_rain_today_applies_only_for_the_current_local_day() {
        let store = TempestStore::new();
        let now = chrono::Utc::now().timestamp();
        // A reading from two days ago must not seed today's bucket.
        assert!(!store.seed_rain_today(0.5, now - 2 * 86_400));
        // Today's persisted total seeds the accumulator.
        assert!(store.seed_rain_today(0.5, now));
        // A live packet accumulates ON TOP of the seed (0.5" + 0.10").
        store.apply_obs("ST", "HB", &obs(now, 2.54));
        assert!((store.snapshot().rain_in_today - 0.60).abs() < 1e-6);
        // A smaller re-seed never lowers the accumulator.
        assert!(!store.seed_rain_today(0.10, now));
        // Zero / negative seeds are rejected outright.
        assert!(!store.seed_rain_today(0.0, now));
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
        let store = TempestStore::new();
        let t0 = 1_700_000_000;
        store.apply_strike(&strike(t0, 5.0));
        store.apply_strike(&strike(t0 + 60, 8.0));
        assert_eq!(store.snapshot().lightning_strikes_last_hour, 2);
        // A strike >1h later evicts both earlier ones.
        store.apply_strike(&strike(t0 + 3700, 12.0));
        let snap = store.snapshot();
        assert_eq!(snap.lightning_strikes_last_hour, 1);
        assert_eq!(snap.lightning_recent.len(), 1);
        assert_eq!(snap.last_strike_epoch, Some(t0 + 3700));
    }

    #[test]
    fn ring_caps_at_500_strikes() {
        // A Blitzortung-scale burst (600 strikes inside the hour) must
        // not grow the snapshot beyond the cap; the oldest fall off.
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        let store = TempestStore::new();
        let t = 1_700_000_000;
        store.apply_strike(&located(111, t, 28.5, -81.4, 40.0));
        // Refinement: same id, moved a few km, arrives in a later batch.
        store.apply_strike(&located(111, t, 28.55, -81.35, 41.0));
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
        store.apply_strike(&located(222, t + 1, 28.6, -81.3, 42.0));
        assert_eq!(store.snapshot().lightning_recent.len(), 2);

        // id == 0 (Tempest distance rings) is never deduped even at the
        // same epoch: two rings stay two.
        store.apply_strike(&strike(t + 2, 5.0));
        store.apply_strike(&strike(t + 2, 6.0));
        assert_eq!(store.snapshot().lightning_recent.len(), 4);
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let store = TempestStore::new();
        let mut rx = store.subscribe();
        rx.mark_unchanged();
        store.apply_strikes(&[]);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(store.snapshot().last_strike_epoch, None);
    }
}

/// NWS heat-index formula above 80 °F / 40% RH; NWS wind-chill below 50 °F
/// with wind ≥ 3 mph; otherwise just the air temperature.
#[cfg(feature = "ssr")]
fn feels_like_f(t_f: f64, rh: f64, wind_mph: f64) -> f64 {
    if t_f >= 80.0 && rh >= 40.0 {
        -42.379 + 2.049_015_23 * t_f + 10.143_331_27 * rh
            - 0.224_755_41 * t_f * rh
            - 0.006_837_83 * t_f * t_f
            - 0.054_817_17 * rh * rh
            + 0.001_228_74 * t_f * t_f * rh
            + 0.000_852_82 * t_f * rh * rh
            - 0.000_001_99 * t_f * t_f * rh * rh
    } else if t_f <= 50.0 && wind_mph >= 3.0 {
        35.74 + 0.6215 * t_f - 35.75 * wind_mph.powf(0.16) + 0.4275 * t_f * wind_mph.powf(0.16)
    } else {
        t_f
    }
}

#[cfg(all(test, feature = "ssr"))]
mod bridge_tests {
    use super::*;
    use crate::ports::weather_source::WeatherField as F;

    #[test]
    fn forecast_source_populates_display_but_not_liveness() {
        // The whole containment for Issue 2: a forecast source (live_current=
        // false) fills the dashboard display fields but must NOT stamp
        // last_packet_epoch, or resolve_current_conditions mislabels it as a
        // live station and feeds forecast numbers into a run/skip decision.
        let store = TempestStore::new();
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
        let store = TempestStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 2_000, true, "test");
        let s = store.snapshot();
        assert_eq!(s.air_temp_f, 70.0);
        assert_eq!(s.last_packet_epoch, 2_000);
    }

    #[test]
    fn carries_unset_fields_forward() {
        let store = TempestStore::new();
        store.apply_source_fields(&[(F::AirTempF, 60.0)], 1_000, true, "test");
        store.apply_source_fields(&[(F::RhPct, 40.0)], 1_001, true, "test");
        let s = store.snapshot();
        assert_eq!(s.air_temp_f, 60.0, "temp survives a humidity-only update");
        assert_eq!(s.rh_pct, 40.0);
    }

    #[test]
    fn wind_maps_to_avg_and_structured_forecast_ignored() {
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        assert_eq!(s.pop_pct, 65.0);
    }

    #[test]
    fn structured_forecast_only_batch_is_a_noop() {
        // ForecastDaily/Hourly are structured (carried by SourceEvent::Forecast,
        // not this scalar path), so a batch of only those touches nothing.
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        let store = TempestStore::new();
        store.apply_source_fields(&[(F::AirTempF, 70.0)], 1_000, true, "Ecowitt");
        assert_eq!(store.snapshot().source_label, "Ecowitt");
    }

    #[test]
    fn single_live_source_always_owns() {
        // The common case (and Tempest-only setups): one live source keeps
        // owning current conditions on every refresh, unaffected by arbitration.
        let store = TempestStore::new();
        for (i, t) in [70.0, 71.0, 72.0].into_iter().enumerate() {
            store.apply_source_fields(&[(F::AirTempF, t)], 1_000 + i as i64, true, "ecowitt");
            assert_eq!(store.snapshot().air_temp_f, t);
        }
    }

    #[test]
    fn higher_priority_live_source_owns_current() {
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
    fn tempest_keeps_weather_when_partial_gateway_adds_pressure() {
        // Same bug via the real paths: Tempest (UDP apply_obs) + an Ecowitt GW
        // (bus apply_source_fields) that only reports pressure, at equal priority.
        let store = TempestStore::new();
        let mut p = HashMap::new();
        p.insert("Tempest".to_string(), 100);
        p.insert("ecowitt_gw".to_string(), 100);
        store.set_priorities(p);
        let obs = ObsSt {
            time_epoch: 1_000,
            air_temp_c: 22.2,
            rh_pct: 55.0,
            wind_avg_mps: 3.0,
            pressure_mb: 1013.0,
            ..Default::default()
        };
        store.apply_obs("ST-1", "HB-1", &obs);
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
            s.source_label, "Tempest",
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
        let store = TempestStore::new();
        let mut p = HashMap::new();
        p.insert("Tempest".to_string(), 100);
        p.insert("ecowitt_gw".to_string(), 110); // strictly higher -> owns pressure
        store.set_priorities(p);
        let obs = ObsSt {
            time_epoch: 1_000,
            air_temp_c: 22.0,
            rh_pct: 55.0,
            wind_avg_mps: 3.0,
            pressure_mb: 1013.0,
            ..Default::default()
        };
        store.apply_obs("ST-1", "HB-1", &obs);
        store.apply_source_fields(&[(F::PressureInHg, 30.0)], 1_010, true, "ecowitt_gw");
        let prov: std::collections::HashMap<_, _> =
            store.conditions_provenance().into_iter().collect();
        assert_eq!(prov.get("Air temperature"), Some(&"Tempest".to_string()));
        assert_eq!(prov.get("Wind"), Some(&"Tempest".to_string()));
        assert_eq!(prov.get("Humidity"), Some(&"Tempest".to_string()));
        assert_eq!(prov.get("Pressure"), Some(&"ecowitt_gw".to_string()));
    }

    #[test]
    fn apply_obs_respects_higher_priority_bus_owner() {
        // Locks the PROD path: a Tempest packet must not seize a fresher,
        // higher-priority bus owner, and must reclaim once that owner is stale.
        let store = TempestStore::new();
        let mut p = HashMap::new();
        p.insert("ecowitt".to_string(), 70);
        p.insert("Tempest".to_string(), 50);
        store.set_priorities(p);
        store.apply_source_fields(&[(F::AirTempF, 60.0)], 1_000, true, "ecowitt");
        // Tempest packet (prio 50) while Ecowitt (70) is fresh -> suppressed.
        let obs = ObsSt {
            time_epoch: 1_010,
            air_temp_c: 30.0,
            ..Default::default()
        };
        store.apply_obs("ST-1", "HB-1", &obs);
        let s = store.snapshot();
        assert_eq!(
            s.source_label, "ecowitt",
            "Tempest must not seize a fresh higher owner"
        );
        assert!((s.air_temp_f - 60.0).abs() < 0.01);
        // After the owner goes stale (> LIVE_FRESHNESS_SECS), Tempest reclaims.
        let obs2 = ObsSt {
            time_epoch: 1_010 + 601,
            air_temp_c: 20.0, // 68F
            ..Default::default()
        };
        store.apply_obs("ST-1", "HB-1", &obs2);
        let s2 = store.snapshot();
        assert_eq!(
            s2.source_label, "Tempest",
            "Tempest reclaims after the owner is stale"
        );
        assert!((s2.air_temp_f - 68.0).abs() < 0.01);
    }

    #[test]
    fn stale_owner_yields_current_to_other_live_source() {
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
            let store = TempestStore::new();
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
        // The regression guard (fix #5): Open-Meteo current (a forecast fill,
        // live_current=false) writes rain_intensity_in_hr, while a barometer-only
        // LIVE source keeps last_packet_epoch fresh. The whole-snapshot freshness
        // would mislabel the stale cloud rain as a live station rate and could
        // hard-skip a dry day. With a per-field rain_live_epoch set ONLY by live
        // writers, the engine sees NO live current-rain here.
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        // The owner's WIND BUG (fix #2): pin WIND = open_meteo with a per-source
        // max_age of 2100s (its ~1800s refresh cadence). A 60s-cadence Tempest
        // writes between Open-Meteo refreshes. With the OLD hardcoded 600s window
        // the pinned cloud was judged stale at the 600s mark and the wind-shadowed
        // Tempest reclaimed wind; with the per-source max_age it stays the owner.
        let store = TempestStore::new();
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
        // TIER LOCK (fix #4, last-resort backup) + cloud fallback chain (fix #3):
        // pin WIND to a CLOUD. While ANY cloud in the chain is fresh, the field
        // demotes DOWN the cloud chain and the wind-shadowed live Tempest stays
        // blocked. Once the WHOLE cloud tier is stale, the Tempest reclaims as the
        // LAST RESORT (a reading beats "no backup"), and the pinned cloud reclaims
        // the moment it refreshes.
        let store = TempestStore::new();
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
        let store = TempestStore::new();
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
        // CLOUD FALLBACK CHAIN (fix #3a, priority-aware fill): with NO override and
        // no live station, two clouds compete for a field's fill by priority. The
        // higher-priority cloud must win, and a lower one must not steal it just by
        // writing last (the old staleness-only fill let the last writer win).
        let store = TempestStore::new();
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
        let store = TempestStore::new();
        let mut p = HashMap::new();
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
        let store = TempestStore::new();
        let mut p = HashMap::new();
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

    #[test]
    fn override_field_source_map_reports_pinned_owner() {
        // The snapshot's per-field provenance reflects the override owner so the
        // UI shows "Wind: tempest".
        let store = TempestStore::new();
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
            let store = TempestStore::new();
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
        let store = TempestStore::new();
        let mut p = HashMap::new();
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
            let store = TempestStore::new();
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
        let store = TempestStore::new();
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
