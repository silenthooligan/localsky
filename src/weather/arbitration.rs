// Which source owns which reading, right now.
//
// Every source publishes the same fields, and only one of them can be
// the temperature the engine reads. The rules, in order: an operator's
// pin or ordered chain wins if the source it names is fresh; otherwise
// the highest-priority LIVE source that has written inside its own
// freshness window owns the field; otherwise a cloud fills it for
// display, ranked among the clouds by the same priority, and marked so
// nothing mistakes a forecast number for a station reading.
//
// The freshness window is per source. A Tempest reports every minute and
// an authority every half hour, so one window for both either demotes
// the cloud constantly or lets a dead station hold a field for ages.
//
// This was the second of two arbiters. The LAN station wrote the store
// directly through a copy of these rules, which is how a pinned cloud
// could lose a field to a station that was not supposed to have it.

#[cfg(feature = "ssr")]
use std::collections::HashMap;
#[cfg(feature = "ssr")]
use std::sync::Arc;

#[cfg(feature = "ssr")]
use super::live_store::LiveWeatherStore;

/// How long a live station's last reading is considered "fresh" when a source
/// has no configured `max_age_s`. While a source has stamped a field within this
/// window, a same-or-lower contender (and a forecast fill) won't overwrite it
/// (see apply_source_fields). Tempest packets land ~every minute, so 10 minutes
/// tolerates a few missed packets without letting another source take over.
///
/// This is the FALLBACK only: every freshness check now consults the writing
/// source's PER-SOURCE `max_age` (see `LiveWeatherStore::max_age_for`) so a slow
/// cloud source (Open-Meteo / NWS / Met.no refresh every ~30 min) is not judged
/// stale at 10 min and demoted out from under a user pin (the owner's wind bug:
/// a 1800s-cadence pinned cloud lost wind to a 60s Tempest at the 600s mark).
#[cfg(feature = "ssr")]
pub(crate) const LIVE_FRESHNESS_SECS: i64 = 600;

/// The writer LABEL the NOAA MRMS adapter stamps on the snapshot owners maps: the
/// source's stable config id, the same id the region auto-seeder assigns
/// (`region::region_keyless_authority_entries`) and that `cloud_fill_field`
/// records as the fill owner. `max_age_for_field` keys the per-field rain-RATE
/// freshness override on this literal so the tight `MAX_AGE_MRMS_RATE_S` window
/// applies to the MRMS PrecipRate field and to nothing else.
#[cfg(all(feature = "ssr", test))]
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
pub(super) fn field_owner_key(
    f: crate::ports::weather_source::WeatherField,
) -> Option<&'static str> {
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
        // The integrated total is what competes: a station reporting
        // minutes and one reporting a since-midnight figure are two
        // claims on the same reading.
        RainLastMinIn => "rain_in_today",
        WindLullMph => "wind_lull_mph",
        RapidWindMph => "rapid_wind_mph",
        RapidWindBearingDeg => "rapid_wind_dir",
        BatteryV => "battery_v",
        PrecipType => "precip_type",
        RainTypeStr | ForecastDaily | ForecastHourly => return None,
    })
}

/// Public accessor for `field_owner_key`: the stable snapshot-field key the
/// per-field arbiter (and the user override map) tracks ownership under, for a
/// `WeatherField`. `None` for fields that don't ride the scalar snapshot
/// (string / structured-forecast variants). The override-install path in
/// `runtime` uses this to key `set_field_overrides` the same way the arbiter does.
#[cfg(feature = "ssr")]
pub fn override_owner_key(f: crate::ports::weather_source::WeatherField) -> Option<&'static str> {
    field_owner_key(f)
}

/// Whether the owner recorded at `(owner_epoch, owner_label)` is STALE as of
/// `at`, measured against the OWNER's configured `max_age` (per-source, falling
/// back to `LIVE_FRESHNESS_SECS`). Centralizes the freshness comparison so every
/// arbitration path (live claim, forecast fill, cloud fill, override decision)
/// judges a source by ITS OWN cadence instead of the one hardcoded 600s window.
#[cfg(feature = "ssr")]
pub(crate) fn owner_is_stale(max_age: i64, owner_epoch: i64, at: i64) -> bool {
    owner_epoch <= 0 || owner_epoch > at || at.saturating_sub(owner_epoch) > max_age
}

#[cfg(feature = "ssr")]
impl LiveWeatherStore {
    /// Install the per-source current-conditions priority map (boot + hot reload). Keys are
    /// the labels writers use, which is each source's config id
    /// otherwise.
    pub fn set_priorities(&self, map: HashMap<String, i32>) {
        self.priorities.store(Arc::new(map));
    }

    /// Priority for a source label, falling back to the default for unlisted
    /// sources (single-source + test setups, where the map is empty).
    pub(super) fn priority_in(priorities: &HashMap<String, i32>, label: &str) -> i32 {
        priorities
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
    pub(super) fn max_age_for(&self, label: &str) -> i64 {
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
    pub(super) fn max_age_for_field(&self, label: &str, field_key: &str) -> i64 {
        if field_key == "rain_intensity_in_hr"
            && self.rain_natures.load().get(label) == Some(&crate::model::RainNature::RadarQpe)
        {
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
    pub(super) fn live_claim_field(
        &self,
        owners: &mut std::collections::HashMap<&'static str, (i32, i64, String)>,
        key: &'static str,
        priorities: &HashMap<String, i32>,
        at: i64,
        now: i64,
        label: &str,
    ) -> bool {
        let p = Self::priority_in(priorities, label);
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
            // An incumbent's stored rank describes its previous sample, not
            // current configuration. Rank BOTH sources using the same packet's
            // map so a silent demoted owner cannot retain its former rank.
            Some((_, oe, ol)) => {
                p > Self::priority_in(priorities, ol)
                    || owner_is_stale(self.max_age_for(ol), *oe, now)
            }
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
    pub(super) fn forecast_may_fill(
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
    pub(super) fn cloud_tier_fresh(
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

    /// CLOUD-FILL arbitration, a priority-aware fallback chain. Operates
    /// ONLY on `fill_owners` and NEVER on the live tier: a cloud source claims a
    /// field's FILL if no cloud owns it, IT already owns it (refresh), its priority
    /// is STRICTLY higher than the current cloud owner's, or that cloud owner went
    /// stale past ITS `max_age` (demote to the next-highest cloud). Mirrors
    /// `live_claim_field` but records nothing in `field_owners` and sets no live
    /// epoch / `has_live_station` / `last_packet_epoch`, so a cloud fill never
    /// reads as a live station. The caller (the `live_current == false` branch of
    /// `apply_source_fields`) still gates the actual snapshot write through
    /// `forecast_may_fill` so a fresh LIVE station is never overwritten by a fill.
    ///
    /// Recording nothing in `field_owners` is not the same as leaving it alone:
    /// once a cloud write is allowed to take a field, the caller REMOVES the live
    /// entry for that field, so the live tier never keeps naming a station for a
    /// value a cloud produced. That matters most on the path this function does
    /// NOT gate: a pin/chain (`override_decision` -> `Some(true)`) hands a cloud a
    /// field whose live owner is still fresh, and only the caller's clear stops
    /// `rain_today_owner` / `et0_today_is_live` reporting that station as the
    /// fresh live owner of a modelled number.
    #[cfg(feature = "ssr")]
    pub(super) fn cloud_fill_field(
        &self,
        fills: &mut std::collections::HashMap<&'static str, (i32, i64, String)>,
        key: &'static str,
        priorities: &HashMap<String, i32>,
        at: i64,
        now: i64,
        label: &str,
    ) -> bool {
        let p = Self::priority_in(priorities, label);
        let claim = match fills.get(key) {
            None => true,
            Some((_, _, ol)) if ol == label => true,
            // The current owner's staleness is judged by its PER-FIELD window, so
            // a silent MRMS PrecipRate rate (`rain_intensity_in_hr`) demotes to the
            // next cloud (Open-Meteo) in ~15 min instead of inheriting MRMS's wide
            // 2 hr accumulation window. Every other field delegates to the
            // source-level window unchanged.
            Some((_, oe, ol)) => {
                p > Self::priority_in(priorities, ol)
                    || owner_is_stale(self.max_age_for_field(ol, key), *oe, now)
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
    /// (a source id) so the override
    /// compares directly against the label each writer carries. An empty map
    /// disables overrides entirely (the priority merge is unchanged).
    pub fn set_field_overrides(&self, map: HashMap<&'static str, String>) {
        self.field_overrides.store(Arc::new(map));
    }

    /// Install the per-field user PRIORITY CHAINS (boot + hot-reload). Keys are
    /// the same snapshot-field keys the arbiter tracks ownership under (see
    /// `field_owner_key`); values are the ORDERED list of writer LABELs, primary
    /// first (each a source id), so the
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
    pub(super) fn field_chain_for(&self, key: &str) -> Option<Vec<String>> {
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
    ///                      This is the ONE path that hands a field to a writer
    ///                      over a still-fresh owner of the other tier, so the
    ///                      caller must re-record ownership to match the writer:
    ///                      a live winner takes `field_owners`, a cloud winner
    ///                      takes `fill_owners` AND clears `field_owners` for the
    ///                      field (see `apply_source_fields`).
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
    /// FRESHNESS is judged by EACH chain entry's own `max_age`: a
    /// 1800s-cadence cloud entry (configured ~2100) keeps the field well past 600s,
    /// fixing the owner's wind-pin demote. Every chain entry that writes stamps its
    /// own `(field, label)` freshness + tier in `field_override_seen`, so the
    /// "first fresh entry" is decided against each entry's real last-write epoch.
    ///
    /// NEVER-BLANK INVARIANT, with a last-resort backup: when the WHOLE
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
    pub(super) fn override_decision(
        &self,
        key: &'static str,
        label: &str,
        at: i64,
        now: i64,
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
                if owner.is_none()
                    && !owner_is_stale(self.max_age_for_field(entry, key), epoch, now)
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
}
