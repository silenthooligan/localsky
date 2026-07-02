// /api/health endpoint. Liveness + readiness probe with structured
// per-subsystem and per-source status. The health endpoint is always
// reachable, even when the engine is degraded; orchestrators (Docker
// healthcheck, Kubernetes probes, uptime-kuma) hit it to decide
// restart policy.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::{extract::State, response::Json};
use serde::Serialize;

use crate::config::schema::SourceKind;
use crate::config::FileConfigStore;
use crate::forecast::ForecastStore;
use crate::ha::IrrigationStore;
use crate::persistence::SensorHistoryStore;
use crate::ports::config_store::ConfigStore;
use crate::tempest::state::TempestStore;

static STARTED_AT: OnceLock<Instant> = OnceLock::new();

fn started_at() -> Instant {
    *STARTED_AT.get_or_init(Instant::now)
}

/// Hard-offline window. A source is only `offline` (a genuine fetch fault that
/// degrades the instance) when it has had NO Reachability AND NO Observation for
/// this long. A reachable-but-quiet rain authority (a dry / no-coverage MRMS
/// that fetches both grids fine every few minutes and correctly declines to
/// fabricate a dry 0) is `watching`, never `offline`. 1800s (30 min) is long
/// enough that a couple of missed polls on a slow-cadence cloud never trips a
/// false fault, while a real connectivity outage still surfaces within the
/// half-hour. Matches the spec's "no Reachability for some long window like
/// 30+ min" definition of a genuine fault.
const HARD_OFFLINE_WINDOW_S: i64 = 1800;

/// Shared per-source last-REACHABLE map, the reachability twin of
/// `sources::SourceLastSeen`. Defined in the `sources` layer (next to
/// `SourceLastSeen`) so the bus recorder can record into it without `sources`
/// depending on `api`; re-exported here for ergonomics. The adapters publish a
/// `SourceEvent::Reachability` on every successful fetch (e.g. noaa_mrms.rs,
/// nws.rs); the bus recorder stamps the receive epoch on a
/// `Reachability { reachable: true }` event so the honest-status taxonomy can
/// tell a reachable-but-quiet source (a dry rain authority emitting no
/// Observation) apart from a genuinely unreachable one. main.rs threads one
/// handle into both `HealthState.source_reachable` and the runtime so
/// /api/config reads the same map. When the handle is absent (`None`), the
/// taxonomy falls back to the Observation last-seen as the reachability proxy,
/// preserving the previous behavior on an un-wired build.
pub use crate::sources::SourceReachability;

/// The honest per-source status taxonomy (spec 1.6). Computed SERVER-SIDE in one
/// shared fn (`compute_source_status`) from Reachability + `field_sources`
/// ownership + per-field freshness + the catalog, NOT from `now - last_seen`
/// bucketing. The SAME enum + fn feed BOTH /api/health and the
/// /api/config/source_catalog per-source payload, so the row UI and the health
/// rollup read one source of truth.
///
/// Only `Offline` (a genuine fetch fault) participates in the /api/health
/// degraded rollup; `Watching` / `Standby` / `FallingThrough` are CALM and never
/// degrade. CONTRACT OUT (wire strings, snake_case): `active`, `watching`,
/// `standby`, `falling_through`, `offline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    /// Owns a live field per the snapshot `field_sources` right now.
    Active,
    /// Last fetch was reachable (a Reachability event), it emits no Observation
    /// this cycle (dry / no rain / no coverage), and it is NOT past the
    /// hard-offline window. A reachable-but-quiet rain authority is `watching`,
    /// never `offline`.
    Watching,
    /// Enabled + reachable + owns no field, because a higher-priority enabled
    /// source currently owns the field(s) it could provide.
    Standby,
    /// It was the owner, has gone stale past its per-field freshness window, and
    /// another source has taken the field (the chain handled it).
    FallingThrough,
    /// No Reachability AND no Observation past the hard-offline window: a genuine
    /// fault. The ONLY state that degrades the instance.
    Offline,
}

impl SourceStatus {
    /// snake_case wire string, congruent with the `#[serde]` rename above so the
    /// degraded-rollup comparison and the JSON payload can never drift.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceStatus::Active => "active",
            SourceStatus::Watching => "watching",
            SourceStatus::Standby => "standby",
            SourceStatus::FallingThrough => "falling_through",
            SourceStatus::Offline => "offline",
        }
    }
}

/// The reachability + ownership + freshness facts the status taxonomy decides on,
/// gathered per source so `compute_source_status` stays a pure, fully-testable
/// function shared by /api/health and /api/config/source_catalog.
#[derive(Debug, Clone, Copy)]
pub struct SourceStatusInputs {
    /// Effectively enabled (entry-level enabled AND any inner opt-in). A disabled
    /// source is never `active`/`watching`/`standby`; the catalog maps it to the
    /// homeowner "Off"/"Add key" words on its own.
    pub enabled: bool,
    /// This source owns at least one live field per the snapshot `field_sources`
    /// right now. Drives `Active`.
    pub owns_field: bool,
    /// A field this source could provide is CURRENTLY owned by a DIFFERENT source.
    /// With `owns_field == false`, distinguishes a source that has lost / not won a
    /// field it could provide (`FallingThrough` if it was the owner) from a quiet
    /// no-coverage source. NOT priority-aware on its own; see
    /// `outranked_by_higher_priority_owner` for the standby-vs-watching split.
    pub other_owns_a_field_it_could_provide: bool,
    /// PRIORITY-AWARE: a field this source could provide is CURRENTLY owned by a
    /// DIFFERENT source that is strictly HIGHER priority than this source. This is
    /// the ONLY signal that drives `Standby` (genuinely outranked). When a field it
    /// could provide is owned only by LOWER-or-equal-priority sources, this is
    /// false and a reachable non-owner reads `Watching` (it is quiet and should be
    /// winning, e.g. MRMS quiet while lower-priority Open-Meteo covers the rain),
    /// never `Standby`.
    pub outranked_by_higher_priority_owner: bool,
    /// This source was the owner and has now gone stale past its per-field
    /// freshness window while another source took over. Drives `FallingThrough`.
    pub was_owner_now_fell_through: bool,
    /// Epoch the source was last REACHABLE (a Reachability event), or None.
    /// Judged against the fixed `HARD_OFFLINE_WINDOW_S` (30 min): a Reachability
    /// fires on EVERY successful fetch (every few minutes), so a long silence on
    /// THIS channel is a genuine fetch fault.
    pub last_reachable_epoch: Option<i64>,
    /// Epoch of the source's last Observation, or None. The fallback liveness
    /// proxy when the reachability handle is un-wired or an adapter emits data
    /// but no explicit Reachability. Judged against `obs_alive_window_s` (NOT the
    /// 30-min reachability window) because an Observation only fires when the
    /// source has something to report, on its own (possibly slow) cadence.
    pub last_obs_epoch: Option<i64>,
    /// The window (seconds) within which a recent Observation still proves the
    /// source alive, when used as the reachability fallback. Kind-aware, carrying
    /// the slow-cloud cadence (a polled forecast model refreshes every 10-30 min,
    /// so a healthy cloud between polls must not read `offline` on the obs
    /// fallback). The reachability channel keeps the tight 30-min window above;
    /// this only widens the OBSERVATION fallback so an un-wired build does not
    /// false-fault a slow cloud.
    pub obs_alive_window_s: i64,
    /// `now` epoch, for the window comparisons.
    pub now: i64,
}

/// THE shared status fn (spec 1.6). Computes the 5-state taxonomy from
/// Reachability + ownership + per-field freshness + catalog facts, NOT from
/// `now - last_seen`. Used by BOTH /api/health and /api/config/source_catalog so
/// the row UI and the health rollup are congruent.
///
/// Decision order:
///   1. owns a field            -> `Active`.
///   2. reachable recently      -> `FallingThrough` if it just lost a field it
///                                 owned, else `Standby` ONLY if a strictly
///                                 HIGHER-priority source owns a field it could
///                                 provide (genuinely outranked), else `Watching`
///                                 (reachable + quiet, the calm dry-authority
///                                 case, including a reachable non-owner whose
///                                 field is held only by a LOWER-or-equal-priority
///                                 source: it is quiet and should be winning). A
///                                 reachable-but-quiet source is NEVER offline.
///   3. an Observation within the hard-offline window also counts as alive (the
///      legacy reachability proxy for adapters that emit data but no Reachability
///      yet, and the fallback when the reachability handle is un-wired): same
///      FallingThrough / Standby / Watching split.
///   4. otherwise               -> `Offline` (the only degrading state).
///
/// A NOT-effectively-enabled source is not participating in the merge, so the
/// live taxonomy does not apply: it cannot own a field and is reported `offline`
/// (a non-running source). Both surfaces override this for a deliberately-off
/// source with the homeowner "Off" / "Add key to turn on" / "Not in your area"
/// words off the catalog's `enabled` + `key_tier` + region flags, and the
/// /api/health degraded rollup only counts an `offline` source that is ALSO
/// enabled, so an off source never degrades the instance.
pub fn compute_source_status(i: SourceStatusInputs) -> SourceStatus {
    if !i.enabled {
        return SourceStatus::Offline;
    }
    if i.owns_field {
        return SourceStatus::Active;
    }
    // Reachable per a Reachability event within the hard-offline window.
    let reachable_recently = i
        .last_reachable_epoch
        .map(|e| i.now - e <= HARD_OFFLINE_WINDOW_S)
        .unwrap_or(false);
    // An Observation within the kind-aware `obs_alive_window_s` also proves the
    // source is alive: it is the reachability proxy for an un-wired build (no
    // reachability handle) or an adapter that emits data but no explicit
    // Reachability. Judged on the slow-cloud window, NOT the tight 30-min
    // reachability window, so a healthy cloud between polls is never false-faulted.
    let observing_recently = i
        .last_obs_epoch
        .map(|e| i.now - e <= i.obs_alive_window_s)
        .unwrap_or(false);
    if reachable_recently || observing_recently {
        if i.was_owner_now_fell_through {
            SourceStatus::FallingThrough
        } else if i.outranked_by_higher_priority_owner {
            // A strictly HIGHER-priority source owns a field this one could
            // provide: it is genuinely outranked and waiting -> standby.
            SourceStatus::Standby
        } else {
            // Reachable and quiet: nothing to report this cycle (dry / no rain /
            // no coverage). The calm dry-authority case, never a fault. This is
            // ALSO where a reachable non-owner whose field is held only by a
            // LOWER-or-equal-priority source lands: it is quiet and should be
            // winning (e.g. priority-75 MRMS quiet while priority-50 Open-Meteo
            // covers the rain fill), which is `watching`, never `standby`.
            SourceStatus::Watching
        }
    } else {
        SourceStatus::Offline
    }
}

/// The two ownership facts the status taxonomy needs (`owns_field` +
/// `other_owns_a_field_it_could_provide`), computed the SAME way for BOTH
/// /api/health and /api/config/source_catalog so the surfaces cannot drift.
///
/// Ownership is decided by the source's WRITER LABEL (the label the merge
/// actually stamps into `field_provenance`: `TEMPEST_LABEL` for the UDP path, the
/// config id otherwise), tested against:
///   * `owner_labels`: the COMPLETE set of writer labels currently attributed an
///     owned field by the merge (`TempestStore::current_owner_labels`, ALL fields
///     not just the headline subset). `owns_field` is true iff the writer label is
///     in this set, so a source owning ONLY a non-headline field (e.g. soil) is
///     still recognized as `active`.
///   * `field_owners`: field name -> owner WRITER LABEL (the snapshot's
///     `field_sources`, whose values are the raw provenance labels). A field this
///     source could provide (`providable`) whose current owner is a DIFFERENT
///     label drives `other_owns_a_field_it_could_provide`.
///
/// Both call sites pass the SAME writer label (via `runtime::writer_label`) so the
/// previous bug (health matched a friendly display name that never equals the
/// writer's stamped label) cannot recur on either surface. PRIORITY: both call
/// sites ALSO pass the SAME `crate::runtime::source_priority_map` (keyed by writer
/// label) and this source's own priority, so the standby-vs-watching decision is
/// priority-aware and congruent across the two surfaces.
pub struct OwnershipFacts {
    pub owns_field: bool,
    pub other_owns_a_field_it_could_provide: bool,
    pub outranked_by_higher_priority_owner: bool,
}

/// Compute the ownership facts (`owns_field` +
/// `other_owns_a_field_it_could_provide` + the priority-aware
/// `outranked_by_higher_priority_owner`) for a source from its writer label, its
/// own priority, the complete owner-label set, the per-field owner map, the fields
/// the source could provide, and the source priority map. THE single ownership
/// check both /api/health and /api/config/source_catalog call, so the two surfaces
/// stay congruent.
///
/// PRIORITY-AWARENESS (the standby-vs-watching split): `field_owners` values are
/// raw WRITER LABELS (the merge's `field_provenance`), exactly the keys of
/// `priorities` (`crate::runtime::source_priority_map`), so an owner's priority is
/// a direct lookup. `outranked_by_higher_priority_owner` is true ONLY when a field
/// this source could provide is owned by a DIFFERENT writer whose priority is
/// strictly GREATER than `own_priority`. A field held only by a
/// LOWER-or-equal-priority owner leaves it false, so a reachable non-owner that
/// should be winning reads `Watching`, never `Standby` (the MRMS-quiet case: MRMS
/// priority 75 quiet while Open-Meteo priority 50 covers the rain fill). An owner
/// absent from `priorities` is treated as priority 0 (a disabled / unranked
/// writer cannot outrank an enabled source).
pub fn source_ownership_facts(
    writer_label: &str,
    own_priority: i32,
    owner_labels: &std::collections::BTreeSet<String>,
    field_owners: &std::collections::BTreeMap<String, String>,
    providable: &std::collections::HashSet<&str>,
    priorities: &std::collections::HashMap<String, i32>,
) -> OwnershipFacts {
    // owns_field: the writer label appears anywhere in the COMPLETE owner set
    // (any field, headline or not), so a soil-only owner reads `active`.
    let owns_field = owner_labels.contains(writer_label);
    // A field this source could provide that a DIFFERENT writer label owns now:
    // it has lost / not won that field (drives `FallingThrough` on the health path
    // when it was the owner and is still emitting).
    let other_owns_a_field_it_could_provide = field_owners
        .iter()
        .any(|(field, owner)| owner != writer_label && providable.contains(field.as_str()));
    // PRIORITY-AWARE standby gate: a field it could provide owned by a DIFFERENT
    // writer of strictly HIGHER priority. Only this drives `Standby`; a field held
    // by a lower-or-equal owner leaves a reachable non-owner reading `Watching`.
    let outranked_by_higher_priority_owner = field_owners.iter().any(|(field, owner)| {
        owner != writer_label
            && providable.contains(field.as_str())
            && priorities.get(owner).copied().unwrap_or(0) > own_priority
    });
    OwnershipFacts {
        owns_field,
        other_owns_a_field_it_could_provide,
        outranked_by_higher_priority_owner,
    }
}

#[derive(Clone)]
pub struct HealthState {
    pub config_store: Option<Arc<FileConfigStore>>,
    /// When set, /api/health enumerates sources from the loaded config
    /// and reports per-source freshness (seconds since last observation).
    /// Used as a fallback for kinds without an in-memory store (MQTT,
    /// HTTP webhook, Ecowitt local POST receiver).
    pub sensor_history: Option<SensorHistoryStore>,
    /// Live freshness sources for the two legacy v0.1 paths that do not
    /// publish on the source bus: TempestUdp feeds TempestStore via the
    /// UDP listener and OpenMeteo feeds ForecastStore via the refresher.
    /// Every other kind reports freshness from data it actually produced
    /// (the bus recorder's last-seen map + sensor_history rows).
    pub tempest_store: Option<Arc<TempestStore>>,
    pub forecast_store: Option<Arc<ForecastStore>>,
    pub irrigation_store: Option<Arc<IrrigationStore>>,
    /// In-memory per-source last-observation map fed by the bus
    /// recorder (this boot only; sensor_history covers across restarts).
    pub source_last_seen: Option<crate::sources::SourceLastSeen>,
    /// In-memory per-source last-REACHABLE map fed by the bus recorder from the
    /// `Reachability` events the adapters publish on every successful fetch. The
    /// honest-status taxonomy reads THIS (not the Observation last-seen) so a
    /// reachable-but-quiet rain authority reads `watching`, never `offline`.
    /// `None` on an un-wired build, in which case the taxonomy falls back to the
    /// Observation last-seen as the reachability proxy. SEAM (Foundation):
    /// main.rs constructs one `SourceReachability`, threads it into the bus
    /// recorder (record on `Reachability { reachable: true }`) and into this
    /// field + the runtime handles so /api/config can read the same map.
    pub source_reachable: Option<SourceReachability>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub config_present: bool,
    pub version: &'static str,
    pub schema_version: Option<u32>,
    pub uptime_s: u64,
    pub subsystems: SubsystemReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceFreshness>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<ControllerSummary>,
    /// Configured soil probes with no valid reading for 24h+ (see the
    /// refresher's probe-fault detection). Non-empty marks the overall
    /// status degraded so the UI health banner surfaces the dead
    /// hardware. Same anonymous-caller trimming as sources/controllers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub soil_probe_faults: Vec<crate::ha::snapshot::SoilProbeFault>,
    /// The Home Assistant relationship, both directions. None for
    /// anonymous callers on auth-required instances (same trimming as
    /// sources/controllers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ha: Option<HaIntegration>,
    /// Per-field current-conditions provenance: which source currently drives
    /// each headline reading (temp/wind from a station, pressure from a gateway,
    /// etc.). Lets the operator see the per-field merge at work. Same
    /// anonymous-caller trimming as sources/controllers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ConditionsProvenance>,
}

#[derive(Debug, Serialize)]
pub struct ConditionsProvenance {
    /// Display name of the reading (e.g. "Air temperature", "Pressure").
    pub field: &'static str,
    /// The source label currently driving that reading.
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct HaIntegration {
    /// HA_URL env present (the inbound snapshot bridge is configured).
    pub env_configured: bool,
    /// Last poll of HA succeeded (from the irrigation snapshot).
    pub reachable: bool,
    /// Where the irrigation snapshot comes from: "home_assistant" |
    /// "standalone" (deployment.mode resolved against the env).
    pub snapshot_source: &'static str,
    /// ha_passthrough sources: (id, mapped-field count). A zero count
    /// means the source feeds nothing.
    pub passthrough_sources: Vec<(String, usize)>,
    /// Controllers actuated through HA service calls.
    pub service_call_controllers: Vec<String>,
    /// Outbound: MQTT discovery publishing enabled.
    pub mqtt_discovery: bool,
    /// Outbound: epoch of the HA integration's last contact (manifest
    /// fetch at load, or live-stream connect). 0 = never this boot.
    pub hacs_last_seen_epoch: i64,
    /// Outbound: the integration is holding a live SSE stream right now.
    pub hacs_streaming: bool,
}

#[derive(Debug, Serialize)]
pub struct SubsystemReport {
    pub config_store: &'static str,
    pub persistence: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SourceFreshness {
    pub id: String,
    pub kind: &'static str,
    pub enabled: bool,
    /// Diagnostics, KEPT from the old model: the Observation last-seen epoch and
    /// how long ago that was. No longer drive the status bucketing (the honest
    /// taxonomy does), but stay on the wire for operators and dashboards.
    pub last_seen_epoch: Option<i64>,
    pub stale_for_s: Option<i64>,
    /// The honest source-status taxonomy (spec 1.6), one of `active` /
    /// `watching` / `standby` / `falling_through` / `offline`. Computed from
    /// Reachability + `field_sources` ownership + per-field freshness, NOT from
    /// `now - last_seen`. Only `offline` degrades the instance; the rest are
    /// calm. Congruent with the `status` field on each
    /// /api/config/source_catalog cloud entry (same `compute_source_status`).
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ControllerSummary {
    pub id: String,
    pub kind: &'static str,
    pub default: bool,
    pub enabled: bool,
}

pub async fn health(
    State(state): State<HealthState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<HealthResponse> {
    // Anonymous callers on an auth-required instance get liveness only:
    // status/version/uptime, no per-source detail (Docker healthchecks
    // and uptime monitors keep working without leaking topology).
    let full_detail = {
        use crate::auth::middleware::{AuthRequired, RequestIdentity};
        let required = req
            .extensions()
            .get::<AuthRequired>()
            .map(|a| a.0)
            .unwrap_or(false);
        let identified = matches!(
            req.extensions().get::<RequestIdentity>(),
            Some(RequestIdentity::User(_)) | Some(RequestIdentity::TrustedNetwork)
        );
        !required || identified
    };
    let uptime_s = started_at().elapsed().as_secs();
    let mut config_present = false;
    let mut schema_version = None;
    let mut config_status = "missing";
    let mut sources_freshness: Vec<SourceFreshness> = Vec::new();
    let mut controller_summaries: Vec<ControllerSummary> = Vec::new();
    // Source ids that are a passive live-station (TempestUdp) listener on a
    // cloud-only install (never produced a packet, no station present): listed
    // for visibility but excluded from the offline-degradation check below.
    let mut passive_station_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Config-entry id -> friendly display name, built from the loaded config so
    // the conditions-provenance build below can turn the raw provenance label
    // ("open_meteo") into a person-facing name ("Open-Meteo") at THIS
    // presentation boundary, while the merge keeps using the raw id internally.
    // The provenance source is either a config-entry id or the TEMPEST_LABEL;
    // ids that aren't in this map (Tempest, an unknown label) pass through as-is.
    let mut source_friendly: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    if let Some(store) = &state.config_store {
        if store.is_initialized() {
            config_present = true;
            match store.load().await {
                Ok(cfg) => {
                    schema_version = Some(cfg.schema_version);
                    config_status = "ok";

                    // Per-source freshness comes from data the source
                    // actually produced: TempestUdp from the UDP
                    // listener's store, OpenMeteo from the forecast
                    // refresher's store (the two legacy v0.1 paths), and
                    // every bus-publishing kind from the recorder's
                    // last-seen map plus its sensor_history rows
                    // (history survives restarts; the map is this boot).
                    let source_ids: Vec<String> =
                        cfg.sources.iter().map(|s| s.id.clone()).collect();
                    let last_seen = if let Some(hist) = &state.sensor_history {
                        hist.last_seen_per_source(source_ids.clone())
                            .await
                            .unwrap_or_default()
                    } else {
                        std::collections::HashMap::new()
                    };
                    let tempest_last = state
                        .tempest_store
                        .as_ref()
                        .map(|s| s.snapshot().last_packet_epoch)
                        .filter(|e| *e > 0);
                    // Whether a live LOCAL station is actually present for this
                    // deployment, the SAME predicate the in-page verdict-strip
                    // pill uses (crate::tempest::state::station_present). A
                    // TempestUdp source that has never produced a packet on a
                    // no-station (cloud-only) install must NOT count as
                    // offline/degrading: with nothing transmitting on the passive
                    // listener it would otherwise drive a phantom "tempest_lan
                    // offline / degraded" banner ~60s after first load. Once a
                    // station HAS reported, a subsequent silence is real staleness
                    // and still degrades (handled by the normal status below).
                    let station_present = state
                        .tempest_store
                        .as_ref()
                        .map(|s| {
                            let snap = s.snapshot();
                            crate::tempest::state::station_present(
                                snap.last_packet_epoch,
                                &snap.station_serial,
                            )
                        })
                        .unwrap_or(false);
                    let forecast_last = state
                        .forecast_store
                        .as_ref()
                        .map(|s| s.snapshot().last_refresh_epoch)
                        .filter(|e| *e > 0);
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    // Live per-field ownership off the irrigation snapshot's
                    // `field_sources` (canonical field name -> the DISPLAY LABEL
                    // of the source currently driving it). The honest-status
                    // taxonomy reads ownership off THIS, the same surface the
                    // CloudRow UI reads, so the two are congruent. Values are the
                    // friendly source label ("NWS", "Open-Meteo", "Tempest"), so a
                    // source is matched to its ownership by the SAME
                    // `friendly_source_name(kind)` label this loop already builds
                    // into `source_friendly`.
                    let field_sources: std::collections::BTreeMap<String, String> = state
                        .irrigation_store
                        .as_ref()
                        .map(|s| s.snapshot().field_sources.clone())
                        .unwrap_or_default();
                    // The COMPLETE set of writer labels the merge currently
                    // attributes an owned field, across ALL fields (not just the
                    // ~10 headline scalars `field_sources` surfaces). Read straight
                    // off the live TempestStore so a source owning only a non-
                    // headline field (e.g. an Ecowitt gateway owning soil moisture)
                    // is still recognized as an owner. Empty when no live store is
                    // wired (the same posture that leaves `field_sources` empty).
                    let owner_labels: std::collections::BTreeSet<String> = state
                        .tempest_store
                        .as_ref()
                        .map(|s| s.current_owner_labels())
                        .unwrap_or_default();
                    // The source priority map (writer label -> priority), the SAME
                    // map the merge ranks with and the SAME map /api/config passes,
                    // so the standby-vs-watching decision is priority-aware and
                    // congruent across both surfaces. A reachable non-owner is
                    // `standby` ONLY when a strictly HIGHER-priority source owns a
                    // field it could provide; otherwise it is `watching`.
                    let source_priorities = crate::runtime::source_priority_map(&cfg);
                    // Record any TempestUdp listener that has no station present
                    // (cloud-only install): listed for visibility but excluded
                    // from the offline-degradation check below.
                    for entry in &cfg.sources {
                        if matches!(entry.source, SourceKind::TempestUdp(_)) && !station_present {
                            passive_station_ids.insert(entry.id.clone());
                        }
                        // Record this entry's friendly display name, keyed by its
                        // id, for the conditions provenance build below. The
                        // resolver maps the kind tag to a brand/pretty name; the
                        // raw id stays the merge key everywhere else.
                        source_friendly.insert(
                            entry.id.clone(),
                            crate::components::sources_form::friendly_source_name(
                                source_kind_label(&entry.source),
                            ),
                        );
                        let last_seen_epoch = match &entry.source {
                            SourceKind::TempestUdp(_) => tempest_last,
                            SourceKind::OpenMeteo(_) => forecast_last,
                            _ => {
                                let bus = state
                                    .source_last_seen
                                    .as_ref()
                                    .and_then(|m| m.get(&entry.id));
                                let hist = last_seen.get(&entry.id).copied();
                                match (bus, hist) {
                                    (Some(a), Some(b)) => Some(a.max(b)),
                                    (a, b) => a.or(b),
                                }
                            }
                        };
                        // Diagnostics, kept on the wire: how long since the last
                        // Observation. No longer DRIVES the status (the honest
                        // taxonomy does), but operators and dashboards still read it.
                        let stale_for_s = last_seen_epoch.map(|e| (now - e).max(0));

                        // --- Honest source-status taxonomy (spec 1.6). ---
                        // Ownership is matched by this source's WRITER LABEL (the
                        // label the merge actually stamps into field_provenance:
                        // TEMPEST_LABEL for the UDP path, the config id otherwise),
                        // NOT a friendly display name (which never equals the
                        // writer's label, the bug that read a fresh Tempest as
                        // `falling_through`). The SAME shared helper computes both
                        // ownership facts here and in the catalog so the surfaces
                        // cannot drift.
                        let label = crate::runtime::writer_label(entry);
                        // Fields this source COULD provide as a CURRENT scalar.
                        let providable: std::collections::HashSet<&'static str> =
                            crate::runtime::source_field_names(&cfg, entry)
                                .into_iter()
                                .collect();
                        let crate::api::health::OwnershipFacts {
                            owns_field,
                            other_owns_a_field_it_could_provide,
                            outranked_by_higher_priority_owner,
                        } = crate::api::health::source_ownership_facts(
                            &label,
                            entry.priority,
                            &owner_labels,
                            &field_sources,
                            &providable,
                            &source_priorities,
                        );
                        // Reachability epoch off the dedicated reachability map
                        // (the adapters' Reachability events). When the handle is
                        // un-wired (None), `compute_source_status` falls back to the
                        // Observation last-seen as the reachability proxy.
                        let last_reachable_epoch = state
                            .source_reachable
                            .as_ref()
                            .and_then(|m| m.get(&entry.id));
                        // FallingThrough vs Standby: a source that is STILL emitting
                        // (a recent Observation) yet no longer owns a field it can
                        // provide WAS the owner and the chain moved past it -> it is
                        // falling through. A reachable-but-quiet non-owner (no recent
                        // Observation) whose field a higher source holds is simply
                        // on standby, ready to take over.
                        // Kind-aware OBSERVATION-fallback window: a polled forecast
                        // model refreshes every 10-30 min, so a healthy cloud
                        // between polls must not read `offline` on the obs proxy.
                        // These are the old per-kind `offline_s` values, reused
                        // ONLY for the obs fallback; the reachability channel keeps
                        // the tight 30-min `HARD_OFFLINE_WINDOW_S` inside the fn.
                        let obs_alive_window_s: i64 = match &entry.source {
                            SourceKind::OpenMeteo(_)
                            | SourceKind::Nws(_)
                            | SourceKind::OpenWeather(_)
                            | SourceKind::PirateWeather(_)
                            | SourceKind::MetNorway(_)
                            | SourceKind::WeatherKit(_)
                            | SourceKind::Netatmo(_)
                            | SourceKind::NoaaMrms(_) => 10800,
                            SourceKind::Lacrosse(_) => 3600,
                            _ => HARD_OFFLINE_WINDOW_S,
                        };
                        let observing_recently = last_seen_epoch
                            .map(|e| now - e <= obs_alive_window_s)
                            .unwrap_or(false);
                        let was_owner_now_fell_through =
                            observing_recently && other_owns_a_field_it_could_provide;
                        let status = compute_source_status(SourceStatusInputs {
                            enabled: source_effectively_enabled(entry),
                            owns_field,
                            other_owns_a_field_it_could_provide,
                            outranked_by_higher_priority_owner,
                            was_owner_now_fell_through,
                            last_reachable_epoch,
                            last_obs_epoch: last_seen_epoch,
                            obs_alive_window_s,
                            now,
                        })
                        .as_str();
                        sources_freshness.push(SourceFreshness {
                            id: entry.id.clone(),
                            kind: source_kind_label(&entry.source),
                            // Effective, not entry-level: a parked
                            // blitzortung entry (inner opt-in off) is
                            // intentionally silent and must not trip
                            // the any-offline degraded check below.
                            enabled: source_effectively_enabled(entry),
                            last_seen_epoch,
                            stale_for_s,
                            status,
                        });
                    }

                    for entry in &cfg.controllers {
                        controller_summaries.push(ControllerSummary {
                            id: entry.id.clone(),
                            kind: controller_kind_label(&entry.controller),
                            default: entry.default,
                            enabled: entry.enabled,
                        });
                    }
                }
                Err(_) => {
                    config_status = "error";
                }
            }
        }
    }

    // Home Assistant relationship summary (both directions).
    let ha = {
        let env_configured = std::env::var("HA_URL").is_ok();
        let reachable = state
            .irrigation_store
            .as_ref()
            .map(|s| s.snapshot().ha_reachable)
            .unwrap_or(false);
        let mut passthrough_sources = Vec::new();
        let mut service_call_controllers = Vec::new();
        let mut mqtt_discovery = false;
        let mut mode_standalone = false;
        if let Some(store) = &state.config_store {
            if let Ok(cfg) = store.load().await {
                for e in &cfg.sources {
                    if let SourceKind::HaPassthrough(c) = &e.source {
                        passthrough_sources.push((e.id.clone(), c.field_map.len()));
                    }
                }
                for c in &cfg.controllers {
                    if matches!(
                        c.controller,
                        crate::config::schema::ControllerKind::HaServiceCall(_)
                    ) {
                        service_call_controllers.push(c.id.clone());
                    }
                }
                mqtt_discovery = cfg
                    .notifications
                    .mqtt
                    .as_ref()
                    .map(|m| m.publish_enabled)
                    .unwrap_or(false);
                mode_standalone = matches!(
                    cfg.deployment.mode,
                    crate::config::schema::DeploymentMode::Standalone
                ) || (matches!(
                    cfg.deployment.mode,
                    crate::config::schema::DeploymentMode::Auto
                ) && !env_configured);
            }
        }
        Some(HaIntegration {
            env_configured,
            reachable,
            snapshot_source: if mode_standalone {
                "standalone"
            } else {
                "home_assistant"
            },
            passthrough_sources,
            service_call_controllers,
            mqtt_discovery,
            hacs_last_seen_epoch: crate::api::manifest::LAST_MANIFEST_FETCH_EPOCH
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(
                    crate::api::irrigation::LAST_INTEGRATION_STREAM_EPOCH
                        .load(std::sync::atomic::Ordering::Relaxed),
                ),
            hacs_streaming: crate::api::irrigation::INTEGRATION_STREAMS
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0,
        })
    };

    // Faulted soil probes (computed by the refresher onto the snapshot).
    // Dead hardware degrades the engine's soil awareness, so it degrades
    // the reported status the same way an offline source does.
    let mut soil_probe_faults = state
        .irrigation_store
        .as_ref()
        .map(|s| s.snapshot().soil_probe_faults.clone())
        .unwrap_or_default();

    let status = match (config_present, config_status) {
        (true, "ok") => {
            // ONLY a genuinely-unreachable source (taxonomy `offline`) degrades
            // the instance. The calm states `watching` / `standby` /
            // `falling_through` are the fall-through chain working, NOT a fault,
            // so a dry / no-coverage rain authority that fetches fine and simply
            // emits no Observation reads `watching` here and never reds the
            // instance. The same passive-station exemption still applies: a
            // TempestUdp listener on a cloud-only install (no station ever
            // present) is `offline` only because nothing transmits on its passive
            // socket, so it is excluded from the degrade check (the in-page
            // verdict-strip pill uses the identical station-presence gate).
            let any_offline = sources_freshness.iter().any(|s| {
                s.enabled
                    && s.status == SourceStatus::Offline.as_str()
                    && !passive_station_ids.contains(&s.id)
            });
            if any_offline || !soil_probe_faults.is_empty() {
                "degraded"
            } else {
                "ok"
            }
        }
        (false, _) => "wizard",
        (_, _) => "degraded",
    };

    let mut conditions: Vec<ConditionsProvenance> = state
        .tempest_store
        .as_ref()
        .map(|s| {
            s.conditions_provenance()
                .into_iter()
                .map(|(field, source)| {
                    // Friendly-name the provenance label at this presentation
                    // boundary: a config-entry id ("open_meteo") becomes its
                    // brand name ("Open-Meteo"); the TEMPEST_LABEL and any
                    // unmapped label pass through unchanged. The merge keeps the
                    // raw id internally; only this rendered string is humanized.
                    let source = source_friendly.get(&source).cloned().unwrap_or(source);
                    ConditionsProvenance { field, source }
                })
                .collect()
        })
        .unwrap_or_default();

    let mut ha = ha;
    if !full_detail {
        sources_freshness.clear();
        controller_summaries.clear();
        soil_probe_faults.clear();
        conditions.clear();
        schema_version = None;
        ha = None;
    }

    Json(HealthResponse {
        status,
        config_present,
        version: env!("CARGO_PKG_VERSION"),
        schema_version,
        uptime_s,
        subsystems: SubsystemReport {
            config_store: config_status,
            persistence: "ok",
        },
        sources: sources_freshness,
        controllers: controller_summaries,
        soil_probe_faults,
        ha,
        conditions,
    })
}

use crate::config::kind_labels::source_kind_label;

/// Whether a source entry is actually supposed to be producing data.
/// Most kinds run whenever the entry-level `enabled` is set, but
/// blitzortung adds an inner opt-in (default false, licensing gate): an
/// entry whose inner flag is off is PARKED by design, never connects,
/// and must not count as an offline source that degrades overall
/// health. The shipped UI template creates exactly that parked state.
fn source_effectively_enabled(entry: &crate::config::schema::SourceEntry) -> bool {
    entry.enabled
        && match &entry.source {
            crate::config::schema::SourceKind::Blitzortung(c) => c.enabled,
            _ => true,
        }
}

use crate::config::kind_labels::controller_kind_label;

#[cfg(test)]
mod tests {
    use super::{
        compute_source_status, source_effectively_enabled, source_ownership_facts, SourceStatus,
        SourceStatusInputs, HARD_OFFLINE_WINDOW_S,
    };
    use crate::config::schema::SourceEntry;
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    /// Build a complete owner-label set (mirrors `current_owner_labels`).
    fn owner_set(labels: &[&str]) -> BTreeSet<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    /// Build a per-field owner map (mirrors the snapshot's `field_sources`:
    /// canonical field name -> owning WRITER LABEL).
    fn field_owners(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(f, o)| (f.to_string(), o.to_string()))
            .collect()
    }

    fn providable(fields: &[&'static str]) -> HashSet<&'static str> {
        fields.iter().copied().collect()
    }

    /// Build a source priority map (mirrors `runtime::source_priority_map`: writer
    /// label -> priority), for the priority-aware standby-vs-watching gate.
    fn priorities(pairs: &[(&str, i32)]) -> std::collections::HashMap<String, i32> {
        pairs.iter().map(|(l, p)| (l.to_string(), *p)).collect()
    }

    fn entry(json: serde_json::Value) -> SourceEntry {
        serde_json::from_value(json).unwrap()
    }

    /// Base inputs for an enabled source at `now = 10_000` with nothing set; each
    /// test flips only the fields it exercises so the decision under test is clear.
    fn base_inputs() -> SourceStatusInputs {
        SourceStatusInputs {
            enabled: true,
            owns_field: false,
            other_owns_a_field_it_could_provide: false,
            outranked_by_higher_priority_owner: false,
            was_owner_now_fell_through: false,
            last_reachable_epoch: None,
            last_obs_epoch: None,
            // Obs fallback uses the same tight window as reachability in the base
            // inputs; individual tests that exercise the slow-cloud fallback widen
            // it. Keeps the "no signal" cases deterministic.
            obs_alive_window_s: HARD_OFFLINE_WINDOW_S,
            now: 10_000,
        }
    }

    #[test]
    fn status_enum_serializes_snake_case() {
        // CONTRACT OUT: the UI agents match these exact wire strings.
        for (variant, wire) in [
            (SourceStatus::Active, "active"),
            (SourceStatus::Watching, "watching"),
            (SourceStatus::Standby, "standby"),
            (SourceStatus::FallingThrough, "falling_through"),
            (SourceStatus::Offline, "offline"),
        ] {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(serde_json::to_value(variant).unwrap(), wire);
        }
    }

    #[test]
    fn owns_a_field_is_active() {
        let i = SourceStatusInputs {
            owns_field: true,
            // Even with NO recent reachability/observation, ownership wins: the
            // merge would not show this source as the live owner unless it is.
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Active);
    }

    #[test]
    fn reachable_but_quiet_rain_authority_is_watching_never_offline() {
        // The coastal-MRMS case: reachable a moment ago (Reachability event), no
        // Observation this cycle (no coverage), owns nothing, no field contested.
        // It must read `watching`, the calm dry-authority state, NEVER `offline`.
        let i = SourceStatusInputs {
            last_reachable_epoch: Some(10_000 - 60),
            last_obs_epoch: None,
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Watching);
    }

    #[test]
    fn reachable_non_owner_outranked_is_standby() {
        // Reachable, not emitting right now, a strictly HIGHER-priority source owns
        // a field it could provide: it is genuinely outranked, ready to take over
        // -> standby (calm, non-degrading).
        let i = SourceStatusInputs {
            last_reachable_epoch: Some(10_000 - 60),
            last_obs_epoch: None,
            other_owns_a_field_it_could_provide: true,
            outranked_by_higher_priority_owner: true,
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Standby);
    }

    #[test]
    fn reachable_quiet_higher_priority_over_lower_owner_is_watching_not_standby() {
        // The MRMS-quiet case at the COMPUTE layer: this source is reachable, owns
        // nothing, and a DIFFERENT source owns a field it could provide, BUT that
        // owner is LOWER priority (so `outranked_by_higher_priority_owner` is
        // false). It is quiet and should be winning -> `watching`, never `standby`.
        let i = SourceStatusInputs {
            last_reachable_epoch: Some(10_000 - 60),
            last_obs_epoch: None,
            other_owns_a_field_it_could_provide: true,
            outranked_by_higher_priority_owner: false,
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Watching);
    }

    #[test]
    fn still_emitting_but_lost_the_field_is_falling_through() {
        // It is still producing data (recent Observation) yet another source now
        // owns a field it provides: the chain moved past it -> falling_through.
        let i = SourceStatusInputs {
            last_obs_epoch: Some(10_000 - 60),
            other_owns_a_field_it_could_provide: true,
            was_owner_now_fell_through: true,
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::FallingThrough);
    }

    #[test]
    fn no_reachability_and_no_observation_past_window_is_offline() {
        // A genuine fault: nothing reachable, nothing observed, both past the
        // hard-offline window. This is the ONLY degrading state.
        let stale = 10_000 - HARD_OFFLINE_WINDOW_S - 1;
        let i = SourceStatusInputs {
            last_reachable_epoch: Some(stale),
            last_obs_epoch: Some(stale),
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Offline);
    }

    #[test]
    fn never_seen_is_offline() {
        // No data of any kind ever: offline (a source that should be producing and
        // never has). Ownership is false, so this cannot be active.
        assert_eq!(compute_source_status(base_inputs()), SourceStatus::Offline);
    }

    #[test]
    fn recent_observation_alone_keeps_a_source_alive_when_reachability_unwired() {
        // Fallback path: on an un-wired build (no reachability handle) a source
        // that is plainly emitting Observations must NOT read offline. With no
        // contested field it reads `watching`, not a fault.
        let i = SourceStatusInputs {
            last_reachable_epoch: None,
            last_obs_epoch: Some(10_000 - 60),
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Watching);
    }

    #[test]
    fn slow_cloud_between_polls_is_not_offline_on_obs_fallback() {
        // Regression for the tight-window bug: a healthy cloud that polls every
        // ~30 min, with the reachability handle un-wired (None) and its last
        // Observation 40 min ago, must read alive on the kind-aware obs window
        // (10800s), NOT `offline`. With no contested field it reads `watching`.
        let i = SourceStatusInputs {
            last_reachable_epoch: None,
            last_obs_epoch: Some(10_000 - 2400), // 40 min ago
            obs_alive_window_s: 10_800,          // slow-cloud window
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Watching);
        // Same source past even the wide window IS a genuine fault.
        let stale = SourceStatusInputs {
            last_reachable_epoch: None,
            last_obs_epoch: Some(10_000 - 10_801),
            obs_alive_window_s: 10_800,
            ..base_inputs()
        };
        assert_eq!(compute_source_status(stale), SourceStatus::Offline);
    }

    #[test]
    fn disabled_source_is_offline_regardless_of_signals() {
        // A not-effectively-enabled source is not participating: even reachable
        // and emitting, it reads `offline` (a non-running source). Both surfaces
        // override this for a deliberately-off source via the enabled/key flags,
        // and the degraded rollup only counts an offline source that is ALSO
        // enabled, so an off source never degrades the instance.
        let i = SourceStatusInputs {
            enabled: false,
            last_reachable_epoch: Some(10_000 - 10),
            last_obs_epoch: Some(10_000 - 10),
            ..base_inputs()
        };
        assert_eq!(compute_source_status(i), SourceStatus::Offline);
    }

    #[test]
    fn parked_blitzortung_is_not_effectively_enabled() {
        // Entry-level enabled defaults true; the inner opt-in defaults
        // false. That parked state (exactly what the UI template
        // creates) must not look like an offline source.
        let parked = entry(serde_json::json!({
            "id": "blitz", "kind": "blitzortung", "config": {},
        }));
        assert!(parked.enabled, "entry-level enabled should default true");
        assert!(!source_effectively_enabled(&parked));
        // Both flags on: effectively enabled.
        let live = entry(serde_json::json!({
            "id": "blitz", "kind": "blitzortung", "config": {"enabled": true},
        }));
        assert!(source_effectively_enabled(&live));
        // Other kinds follow the entry-level flag alone.
        let demo = entry(serde_json::json!({
            "id": "demo", "kind": "demo_replay", "config": {},
        }));
        assert!(source_effectively_enabled(&demo));
        let off = entry(serde_json::json!({
            "id": "demo", "kind": "demo_replay", "enabled": false, "config": {},
        }));
        assert!(!source_effectively_enabled(&off));
    }

    // --- Shared ownership helper (the writer-label match + complete owner set). ---

    #[test]
    fn tempest_owned_field_matched_by_writer_label_is_active() {
        // The regression: a fresh Tempest stamps its WRITER LABEL ("Tempest") into
        // field_provenance for temp/wind. The status must match on THAT label, not
        // a friendly display name ("Tempest UDP (LAN)") which never equals it. With
        // the writer-label match the Tempest owns a field -> active.
        let writer = crate::tempest::state::TEMPEST_LABEL; // "Tempest"
        let owners = owner_set(&[writer]);
        let field_owners = field_owners(&[("air_temp_f", writer), ("wind_mph", writer)]);
        let providable = providable(&["air_temp_f", "wind_mph"]);
        let prios = priorities(&[(writer, 90)]);
        let facts = source_ownership_facts(writer, 90, &owners, &field_owners, &providable, &prios);
        assert!(
            facts.owns_field,
            "a Tempest owning temp/wind is matched by its writer label"
        );
        let status = compute_source_status(SourceStatusInputs {
            enabled: true,
            owns_field: facts.owns_field,
            other_owns_a_field_it_could_provide: facts.other_owns_a_field_it_could_provide,
            outranked_by_higher_priority_owner: facts.outranked_by_higher_priority_owner,
            ..base_inputs()
        });
        assert_eq!(
            status,
            SourceStatus::Active,
            "fresh Tempest reads active, not falling_through"
        );
    }

    #[test]
    fn soil_only_owner_is_active_via_complete_owner_set() {
        // An Ecowitt gateway owns ONLY soil moisture, a non-headline field absent
        // from the field_source_map/field_owners headline subset. It is still in
        // the COMPLETE owner-label set, so owns_field is true and it reads active,
        // never mis-classified as falling_through for "owning nothing".
        let writer = "ecowitt_gw";
        let owners = owner_set(&[writer]); // soil owner present in the complete set
        let field_owners = field_owners(&[]); // no HEADLINE field owners surfaced
        let providable = providable(&["air_temp_f", "wind_mph"]);
        let prios = priorities(&[(writer, 80)]);
        let facts = source_ownership_facts(writer, 80, &owners, &field_owners, &providable, &prios);
        assert!(
            facts.owns_field,
            "a soil-only owner is recognized via the complete owner set"
        );
        assert!(
            !facts.other_owns_a_field_it_could_provide,
            "no other source owns a field it could provide"
        );
        assert!(!facts.outranked_by_higher_priority_owner);
        let status = compute_source_status(SourceStatusInputs {
            enabled: true,
            owns_field: facts.owns_field,
            other_owns_a_field_it_could_provide: facts.other_owns_a_field_it_could_provide,
            outranked_by_higher_priority_owner: facts.outranked_by_higher_priority_owner,
            ..base_inputs()
        });
        assert_eq!(status, SourceStatus::Active);
    }

    #[test]
    fn non_owner_outranked_is_standby_not_active() {
        // A reachable source that owns nothing while a DIFFERENT, strictly
        // HIGHER-priority writer owns a field it could provide is on standby (calm,
        // genuinely outranked). other_owns is matched by writer label; the standby
        // gate additionally checks the owner's priority is greater.
        let writer = "open_meteo"; // priority 50
        let owners = owner_set(&["Tempest"]); // a live station owns the fields
        let field_owners = field_owners(&[("wind_mph", "Tempest")]);
        let providable = providable(&["wind_mph"]);
        // Tempest (priority 90) strictly outranks Open-Meteo (priority 50).
        let prios = priorities(&[("Tempest", 90), (writer, 50)]);
        let facts = source_ownership_facts(writer, 50, &owners, &field_owners, &providable, &prios);
        assert!(!facts.owns_field);
        assert!(facts.other_owns_a_field_it_could_provide);
        assert!(
            facts.outranked_by_higher_priority_owner,
            "a higher-priority Tempest owning the field genuinely outranks Open-Meteo"
        );
        let status = compute_source_status(SourceStatusInputs {
            enabled: true,
            owns_field: facts.owns_field,
            other_owns_a_field_it_could_provide: facts.other_owns_a_field_it_could_provide,
            outranked_by_higher_priority_owner: facts.outranked_by_higher_priority_owner,
            last_reachable_epoch: Some(10_000 - 60),
            ..base_inputs()
        });
        assert_eq!(status, SourceStatus::Standby);
    }

    #[test]
    fn quiet_higher_priority_over_lower_owner_is_watching_via_ownership_facts() {
        // The MRMS-quiet case through the SHARED OWNERSHIP HELPER (the precision
        // fix): MRMS (priority 75) is reachable, owns nothing, and the rain fill is
        // owned by a LOWER-priority Open-Meteo (priority 50). other_owns is true
        // (a different writer holds the field) but `outranked_by_higher_priority_owner`
        // is FALSE (the owner is lower priority), so the source reads `watching`
        // (quiet and should be winning), NEVER `standby`.
        let writer = "noaa_mrms"; // priority 75
        let owners = owner_set(&["open_meteo"]);
        let field_owners = field_owners(&[("rain_today_in", "open_meteo")]);
        let providable = providable(&["rain_today_in"]);
        let prios = priorities(&[(writer, 75), ("open_meteo", 50)]);
        let facts = source_ownership_facts(writer, 75, &owners, &field_owners, &providable, &prios);
        assert!(!facts.owns_field);
        assert!(
            facts.other_owns_a_field_it_could_provide,
            "a different writer (Open-Meteo) holds the rain field"
        );
        assert!(
            !facts.outranked_by_higher_priority_owner,
            "the owner is LOWER priority, so MRMS is not outranked"
        );
        let status = compute_source_status(SourceStatusInputs {
            enabled: true,
            owns_field: facts.owns_field,
            other_owns_a_field_it_could_provide: facts.other_owns_a_field_it_could_provide,
            outranked_by_higher_priority_owner: facts.outranked_by_higher_priority_owner,
            last_reachable_epoch: Some(10_000 - 60),
            ..base_inputs()
        });
        assert_eq!(
            status,
            SourceStatus::Watching,
            "a quiet higher-priority source whose field a lower source holds reads watching, not standby"
        );
    }

    #[test]
    fn health_and_catalog_agree_watching_for_mrms_quiet_inputs() {
        // CONGRUENCE for the MRMS-quiet case (the bug): MRMS (priority 75) is
        // reachable, owns nothing, and the rain fill is held by a LOWER-priority
        // Open-Meteo (priority 50). MRMS emits NO Observation this cycle (no
        // coverage), so neither surface asserts `falling_through`:
        //   * /api/health: no recent Observation -> `was_owner_now_fell_through`
        //     is false; reachable + not outranked -> `watching`.
        //   * /api/config catalog: never asserts `falling_through`
        //     (`was_owner_now_fell_through: false`); reachable + not outranked ->
        //     `watching`.
        // Both surfaces call the SAME priority-aware ownership helper with the SAME
        // priority map, so both land on `watching`, never `standby`.
        let writer = "noaa_mrms";
        let owners = owner_set(&["open_meteo"]); // lower-priority Open-Meteo took rain
        let field_owners = field_owners(&[("rain_today_in", "open_meteo")]);
        let providable = providable(&["rain_today_in"]);
        let prios = priorities(&[(writer, 75), ("open_meteo", 50)]);

        let facts = source_ownership_facts(writer, 75, &owners, &field_owners, &providable, &prios);
        assert!(!facts.outranked_by_higher_priority_owner);

        // /api/health path: reachable this cycle, no recent Observation (quiet, no
        // coverage), so it never asserts the prior-owner fall-through.
        let health_status = compute_source_status(SourceStatusInputs {
            enabled: true,
            owns_field: facts.owns_field,
            other_owns_a_field_it_could_provide: facts.other_owns_a_field_it_could_provide,
            outranked_by_higher_priority_owner: facts.outranked_by_higher_priority_owner,
            was_owner_now_fell_through: false, // no recent Observation -> not asserted
            last_reachable_epoch: Some(10_000 - 224), // last reachable 224s ago, live
            last_obs_epoch: None,
            ..base_inputs()
        });
        // /api/config catalog path: ownership + reachability only, never asserts
        // falling_through.
        let catalog_status = compute_source_status(SourceStatusInputs {
            enabled: true,
            owns_field: facts.owns_field,
            other_owns_a_field_it_could_provide: facts.other_owns_a_field_it_could_provide,
            outranked_by_higher_priority_owner: facts.outranked_by_higher_priority_owner,
            was_owner_now_fell_through: false,
            last_reachable_epoch: Some(10_000 - 224),
            last_obs_epoch: None,
            ..base_inputs()
        });
        assert_eq!(
            health_status,
            SourceStatus::Watching,
            "a reachable, quiet, higher-priority MRMS reads watching on /api/health"
        );
        assert_eq!(
            catalog_status,
            SourceStatus::Watching,
            "the catalog reads the SAME calm watching for the MRMS-quiet inputs"
        );
        assert_eq!(
            health_status, catalog_status,
            "both surfaces are congruent (watching) for the MRMS-quiet case"
        );
    }
}
