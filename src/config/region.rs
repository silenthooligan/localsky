// Region-aware default ranking for cloud weather sources.
//
// When a cloud source is synthesized (env_compat) or added by the wizard, its
// per-field merge priority and freshness window are seeded from the researched
// default ranking for the station's region, not from a flat constant. Higher
// priority wins the per-field merge (see SourceEntry::priority), so this is what
// makes a US install prefer NWS over Pirate over Open-Meteo out of the box while
// a Nordic install prefers Met.no, all WITHOUT ever pinning a particular live
// local station: the fallback chain demotes a field through the cloud tier by
// priority and never reverts to an un-chosen local source.
//
// This mirrors the region-aware-default pattern the radar provider already uses
// (radar_catalog::recommended / home_basins resolve a default set from lat/lon);
// the same simple axis-aligned bounding boxes drive the region decision here.
//
// Pure (no env, no I/O): every function is a deterministic map of (kind, lat,
// lon) so the ranking is unit-testable in isolation.

use crate::config::schema::SourceKind;

/// The User-Agent string auto-filled on a synthesized keyless authority source
/// (NWS / Met.no). Both `api.weather.gov` and `api.met.no` require a non-empty,
/// operator-identifying User-Agent in their terms of service, and the config
/// validator (`validate.rs`) rejects an empty `user_agent` outright. A
/// no-hardware user never types one, so the default-on regional stack fills it
/// the same way the UI's "add source" prefill does: the package name + version
/// plus the project URL as the contact. An operator can edit it later in the
/// Sources UI without changing behavior.
pub const KEYLESS_AUTHORITY_USER_AGENT: &str = concat!(
    "localsky/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/silenthooligan/localsky)"
);

/// Coarse home region for the configured station, used only to seed default
/// source priorities. Anything outside the US and the Europe/Nordic boxes is
/// `Global` (the keyless backstop ranking).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// United States incl. Alaska, Hawaii, Puerto Rico / Caribbean territories,
    /// and Guam / Pacific territories. NWS is authoritative here.
    Us,
    /// Europe + the Nordics. Met.no (MET Norway) is authoritative here.
    EuropeNordic,
    /// Everywhere else: keyed global providers lead, Open-Meteo backstops,
    /// NWS stays disabled (US-only coverage).
    Global,
}

/// Axis-aligned point-in-box test (south..=north, west..=east), matching the
/// `radar_catalog::in_box` precedent. Kept local so this module stays
/// self-contained and both-features.
fn in_box(lat: f64, lon: f64, south: f64, north: f64, west: f64, east: f64) -> bool {
    (south..=north).contains(&lat) && (west..=east).contains(&lon)
}

/// Resolve the coarse default-ranking region from a station lat/lon using
/// simple, deliberately generous bounding boxes. US is checked first (its
/// non-contiguous boxes can overlap the broad Pacific/Atlantic spans), then
/// Europe/Nordic, else Global.
pub fn region_for(lat: f64, lon: f64) -> Region {
    // ----- United States (incl. AK / HI / territories) -----
    // CONUS: generous box from the southern tip of Texas/Florida up through the
    // northern border, Atlantic to Pacific.
    let conus = in_box(lat, lon, 24.0, 50.0, -125.0, -66.0);
    // Alaska (incl. the Aleutians crossing the antimeridian on the east side).
    let alaska = in_box(lat, lon, 51.0, 72.0, -170.0, -129.0);
    // Hawaii.
    let hawaii = in_box(lat, lon, 18.0, 23.0, -161.0, -154.0);
    // Puerto Rico + US Virgin Islands / Caribbean territories.
    let caribbean = in_box(lat, lon, 17.0, 19.0, -68.0, -64.0);
    // Guam + CNMI (western Pacific territories, NWS-covered).
    let guam = in_box(lat, lon, 13.0, 21.0, 144.0, 146.0);
    if conus || alaska || hawaii || caribbean || guam {
        return Region::Us;
    }

    // ----- Europe + Nordics -----
    // One generous box covering mainland Europe, the British Isles, Iberia,
    // Italy / the central Mediterranean, and up through Scandinavia / the
    // Nordics (Iceland on the west edge, Finland / the Baltics on the east).
    let europe_nordic = in_box(lat, lon, 35.0, 72.0, -25.0, 45.0);
    if europe_nordic {
        return Region::EuropeNordic;
    }

    Region::Global
}

/// Whether a source kind is APPROPRIATE for the deployment location, the axis
/// the cloud-onboarding UI uses to collapse a region-irrelevant option rather
/// than offer a service that does not meaningfully cover the user's yard.
///
/// Today this gates exactly one case: MET Norway is a coarse 9 km or worse grid
/// outside the Nordics, so it is NOT region-appropriate outside Europe/Nordic.
/// Every other kind is appropriate everywhere (NWS / NOAA MRMS are US-only by
/// `default_enabled_for`, which is the enablement gate; this predicate is the
/// softer UI-collapse signal, and a US user can still see them). This stays
/// deliberately narrow so the UI never hides a working option.
pub fn is_region_appropriate(kind: &SourceKind, lat: f64, lon: f64) -> bool {
    match kind {
        // Met.no is only region-appropriate in its authority region (the
        // Nordics / Europe); elsewhere its grid is too coarse to lead with.
        SourceKind::MetNorway(_) => region_for(lat, lon) == Region::EuropeNordic,
        _ => true,
    }
}

/// True for the cloud forecast kinds whose upstream refresh cadence is ~1800s
/// (Open-Meteo, NWS, MET Norway). Their synthesized `max_age_s` is widened to
/// `MAX_AGE_SLOW_CADENCE_S` so a per-field source pin survives a full refresh
/// cycle (the owner's wind-pin bug: an override gated on 600s expired against an
/// 1800s OM refresh). The keyed 600s-cadence sources keep the kind default.
fn is_slow_cadence(kind: &SourceKind) -> bool {
    use SourceKind::*;
    matches!(kind, OpenMeteo(_) | Nws(_) | MetNorway(_))
}

/// max_age_s seeded on the ~1800s-cadence cloud sources. A hair over the 1800s
/// refresh so a freshly merged value (and any per-field pin riding on it) is
/// still considered fresh when the next refresh lands, closing the wind-pin
/// freshness-cadence mismatch.
pub const MAX_AGE_SLOW_CADENCE_S: u64 = 2100;

/// max_age_s seeded on NOAA MRMS specifically. MRMS now reads TWO products per
/// cycle: an instantaneous PrecipRate rate (valid ~now) AND a gauge-corrected
/// hourly QPE accumulation, and the accumulation is INHERENTLY ~1 to 1.5 hr
/// behind (pass-2 publishes ~80 min late). At the old 2100s (35 min) window that
/// hourly accumulation field always landed STALE in the merge: it decoded but
/// never drove a decision. Widening to 2 hours keeps the lagged accumulation
/// field FRESH long enough to be useful. The PrecipRate rate is valid ~now and
/// refreshed every cycle, so the wider window does not make it stale-tolerant in
/// practice; the adapter's own 45 min reject guards the rate at the source.
pub const MAX_AGE_MRMS_S: u64 = 7200;

/// Per-FIELD freshness window for the MRMS instantaneous PrecipRate RATE
/// (`rain_intensity_in_hr`), which is judged on its OWN ~15 min cadence rather
/// than inheriting the wide `MAX_AGE_MRMS_S` accumulation window above. MRMS
/// publishes PrecipRate ~every 2 min and the adapter rejects an old grid at the
/// source past 45 min, so 15 min is a comfortable "this rate is current" window
/// that tolerates a couple missed cycles. Without this split a no-coverage
/// coastal MRMS rate would freeze the rain owner for up to 2 hours (the 7200s
/// accumulation window); with it the rate goes stale in ~15 min so Open-Meteo
/// (model) takes the rain fill within minutes. The gauge-corrected hourly
/// accumulation field keeps the wide `MAX_AGE_MRMS_S` window (it is inherently
/// ~1 to 1.5 hr late). See `TempestStore::max_age_for_field`.
pub const MAX_AGE_MRMS_RATE_S: u64 = 900;

/// Default `max_age_s` for a synthesized/added cloud source. The slow-cadence
/// forecast kinds (Open-Meteo, NWS, Met.no) get `MAX_AGE_SLOW_CADENCE_S` so a
/// per-field pin outlives the refresh cycle; NOAA MRMS gets the wider
/// `MAX_AGE_MRMS_S` so its lagged hourly-accumulation field stays fresh; the
/// keyed current-conditions providers (OpenWeather, Pirate, WeatherKit) get the
/// 3900s window matching the `/api/health` kind bucket so a normal ~10 to 60 min
/// poll gap does not read stale and flap the rain owner; every other kind keeps
/// `None` (the kind-default freshness window, e.g. live local stations).
pub fn default_max_age_for(kind: &SourceKind) -> Option<u64> {
    // NOAA MRMS reads a gauge-corrected hourly accumulation that is inherently
    // ~1 to 1.5 hr behind, so it needs a wider freshness window than the 1800s
    // forecast kinds or that field always reads stale in the merge. The
    // companion PrecipRate rate is valid ~now and guarded at the source by the
    // adapter's 45 min reject, so the wider window does not make the rate
    // stale-tolerant in practice.
    if matches!(kind, SourceKind::NoaaMrms(_)) {
        Some(MAX_AGE_MRMS_S)
    } else if is_slow_cadence(kind) {
        Some(MAX_AGE_SLOW_CADENCE_S)
    } else if matches!(
        kind,
        SourceKind::OpenWeather(_) | SourceKind::PirateWeather(_) | SourceKind::WeatherKit(_)
    ) {
        // The keyed current-conditions providers refresh on a ~10 to 60 min
        // cadence, so the 600s live-freshness fallback judges a healthy poll
        // stale BETWEEN polls (it loses its fill or gets demoted under a pin;
        // Pirate at US priority 60 even flaps the rain owner). Seed a 65 min
        // window so the merge tolerates a full poll gap. 3900 deliberately
        // matches the `/api/health` kind bucket so the merge and health agree
        // on freshness.
        Some(3900)
    } else {
        None
    }
}

/// Whether a synthesized/added cloud source should be enabled by default for
/// this region. Only NWS is region-gated: it covers US territory only, so it is
/// disabled by default outside the US bbox (a user can still enable it by hand).
/// Every other kind defaults enabled.
pub fn default_enabled_for(kind: &SourceKind, lat: f64, lon: f64) -> bool {
    match kind {
        // NWS and NOAA MRMS are both US-only coverage, so both are disabled by
        // default outside the US bbox (a user can still enable by hand).
        SourceKind::Nws(_) | SourceKind::NoaaMrms(_) => region_for(lat, lon) == Region::Us,
        _ => true,
    }
}

/// The TRUE per-region auto-seeded set: the keyless authority a no-hardware
/// install boots with, zero clicks. This is the SINGLE SOURCE OF TRUTH for what
/// `finalize_sources` (wizard) and `synthesize` (env_compat) actually enable, so
/// any UI "recommended here" / "on for you" marker keys off it and can never
/// claim a service the install does not actually seed.
///
/// The set is, by region:
///   * Open-Meteo  -> ALWAYS (the keyless backstop, seeded everywhere).
///   * NWS         -> only when region == Us (US-only coverage).
///   * Met.no      -> only when region == EuropeNordic (its authority region).
///
/// EVERYTHING else is false here: a keyed/paid provider (OpenWeather, Pirate,
/// WeatherKit) is never auto-seeded (those stay operator opt-in), and a keyless
/// kind outside its region (Met.no in the US, NWS in Europe) is not seeded there.
/// So a "recommended here" badge driven by this never lights on a paid service
/// or on the self-described-weakest Met.no outside the Nordics.
///
/// Matches on the `SourceKind` VARIANT only (never the config payload), so a
/// synthetic catalog instance with placeholder credentials resolves the same as
/// a real configured source.
pub fn is_region_keyless_authority(kind: &SourceKind, lat: f64, lon: f64) -> bool {
    use SourceKind::*;
    match kind {
        // Open-Meteo is the always-on keyless backstop, seeded in every region.
        OpenMeteo(_) => true,
        // NWS: a US keyless authority, seeded only inside the US bbox.
        Nws(_) => region_for(lat, lon) == Region::Us,
        // NOAA MRMS: the US keyless RADAR authority, seeded only inside the US
        // bbox, exactly like NWS (both keyless, both US-only).
        NoaaMrms(_) => region_for(lat, lon) == Region::Us,
        // Met.no: the Europe/Nordic keyless authority, seeded only there.
        MetNorway(_) => region_for(lat, lon) == Region::EuropeNordic,
        // Keyed providers + everything else are never auto-seeded.
        _ => false,
    }
}

/// The `SourceKind` variants the region auto-seeds (the keyless authority set),
/// as freshly built instances ready for a variant-keyed lookup. The companion to
/// [`is_region_keyless_authority`] for a caller that wants to enumerate the set
/// rather than test a single kind. Open-Meteo first (always), then the regional
/// authority (NWS in the US, Met.no in the Nordics, nothing extra in Global).
///
/// The instances carry placeholder configs (the variant is all that matters for
/// the predicate / a region lookup); use [`region_keyless_authority_entries`]
/// when you need persistable `SourceEntry`s with auto-filled user agents.
pub fn region_keyless_authority_kinds(lat: f64, lon: f64) -> Vec<SourceKind> {
    use crate::config::schema::{MetNorwayConfig, NoaaMrmsConfig, NwsConfig, OpenMeteoConfig};
    let mut kinds = vec![SourceKind::OpenMeteo(
        serde_json::from_value::<OpenMeteoConfig>(serde_json::json!({})).expect("serde defaults"),
    )];
    match region_for(lat, lon) {
        Region::Us => {
            kinds.push(SourceKind::Nws(NwsConfig {
                user_agent: KEYLESS_AUTHORITY_USER_AGENT.to_string(),
            }));
            // NOAA MRMS rides alongside NWS in the US: a keyless radar-QPE
            // authority, the best off-yard rain read short of a local gauge.
            kinds.push(SourceKind::NoaaMrms(NoaaMrmsConfig::default()));
        }
        Region::EuropeNordic => kinds.push(SourceKind::MetNorway(MetNorwayConfig {
            user_agent: KEYLESS_AUTHORITY_USER_AGENT.to_string(),
        })),
        Region::Global => {}
    }
    kinds
}

/// Researched default per-field merge priority for a cloud source at this
/// station location. Higher wins. The ranking encodes real-time-quality +
/// regional-authority research:
///
///   US bbox:            NWS 70 > Pirate 60 > OpenWeather/WeatherKit 55 >
///                       Open-Meteo 50 > Met.no 40
///   Europe/Nordic bbox: Met.no 70, the rest as global
///   Global:             WeatherKit/OpenWeather 55 > Pirate 50 = Open-Meteo 50,
///                       NWS disabled (not enabled outside the US bbox)
///
/// Open-Meteo is ALWAYS 50: it is the keyless backstop, the last link in the
/// cloud-only fallback chain, so it must sit below the keyed providers wherever
/// they outrank it and never above a regional authority.
///
/// Kinds that are not cloud forecast sources (local stations, HA passthrough,
/// demo, etc.) fall through to the flat `default_source_priority` value (50);
/// their priority is governed elsewhere (e.g. local stations at 100).
pub fn default_priority_for(kind: &SourceKind, lat: f64, lon: f64) -> i32 {
    use SourceKind::*;
    let region = region_for(lat, lon);

    match kind {
        // Open-Meteo: always the keyless backstop, last link in the chain.
        OpenMeteo(_) => 50,

        // NWS: top STATION authority inside the US; disabled (and so never
        // ranked) outside it. We still return its US rank here for callers that
        // ask for a priority directly; `default_enabled_for` gates enablement.
        Nws(_) => match region {
            Region::Us => 70,
            // Outside the US NWS has no coverage; keep it below every keyed
            // provider so that even if a user force-enables it, it never
            // outranks a source that actually covers their location.
            _ => 30,
        },

        // NOAA MRMS: gauge-corrected radar QPE, the best off-yard rain read
        // short of a local gauge, so it sits ABOVE NWS (70) and below a real
        // on-LAN gauge (100) inside the US. Disabled (and so never ranked)
        // outside it; ranked low there to mirror the NWS out-of-region rule.
        NoaaMrms(_) => match region {
            Region::Us => 75,
            _ => 30,
        },

        // Synoptic (MesoWest): a REAL mesonet station observation (denser than
        // NWS ASOS/AWOS), so it ranks as a station authority (~70) like NWS,
        // above the nowcast/model tiers and below a real on-LAN gauge (100). It
        // has global coverage (not US-only), so the same rank applies in every
        // region; it is a keyed (free-token) source, so it is never auto-seeded.
        Synoptic(_) => 70,

        // Pirate Weather: strong US real-time (NWS-derived nowcast + minutely);
        // a hair below NWS in-US, mid-pack globally.
        PirateWeather(_) => match region {
            Region::Us => 60,
            _ => 50,
        },

        // Apple WeatherKit + OpenWeather: keyed global providers with solid
        // current scalars. Above Open-Meteo everywhere they are chosen.
        WeatherKit(_) | OpenWeather(_) => 55,

        // MET Norway: regional authority in Europe/the Nordics; modest
        // elsewhere (still a valid global model, just not first pick in the US).
        MetNorway(_) => match region {
            Region::EuropeNordic => 70,
            Region::Us => 40,
            Region::Global => 40,
        },

        // Any non-forecast kind keeps the flat schema default (50). Local
        // stations and the like set their own priority at their own call sites.
        _ => 50,
    }
}

/// Apply the region-aware default priority + enablement to every cloud forecast
/// source that is NEW in `cfg` relative to `prev` (added on this write), keyed
/// off the deployment location. This is the server-side counterpart to the
/// wizard/env_compat seeding: the UI "add source" path (sources_form +
/// settings/sources) only has the kind STRING client-side and so seeds a flat
/// priority (50) for every cloud kind, which makes two hand-added clouds tie and
/// fall to first-writer-wins ownership in the merge, and never disables NWS
/// outside the US (it would 404 forever). This normalize closes that gap right
/// before persist.
///
/// CONTRACT (precise + idempotent):
///   * Only sources whose `id` is NOT present in `prev` are touched. An existing
///     source the user customized (any priority/enabled) is left byte-identical
///     on a later save, so a re-save never re-clamps a user's hand-tuned value.
///   * Only the CLOUD FORECAST kinds (`SourceKind::is_forecast`) are normalized.
///     Those are exactly the kinds `default_priority_for` has a researched rank
///     for; local stations / sensor gateways / passthrough keep their own
///     call-site priority (e.g. a LAN station at 100 must never drop to 50).
///   * Priority is set to the region rank. `enabled` is AND-ed with the region
///     gate, so a kind a user explicitly added DISABLED stays disabled, but an
///     enabled NWS added outside the US is flipped off (no permanent 404 / red
///     banner). A user can still re-enable it by hand on a later edit; that
///     later save no longer treats it as new, so the gate does not re-disable it.
///
/// A `prev` of `None` (fresh install, first config write) treats every source as
/// new, which is correct: the very first persisted config should land each cloud
/// at its region rank just as the wizard would have.
pub fn normalize_new_cloud_sources(
    prev: Option<&crate::config::schema::Config>,
    cfg: &mut crate::config::schema::Config,
) {
    use std::collections::HashSet;

    let existing_ids: HashSet<&str> = prev
        .map(|p| p.sources.iter().map(|s| s.id.as_str()).collect())
        .unwrap_or_default();

    let (lat, lon) = (cfg.deployment.location.lat, cfg.deployment.location.lon);

    for entry in cfg.sources.iter_mut() {
        // Skip sources that already existed (preserve the user's customization).
        if existing_ids.contains(entry.id.as_str()) {
            continue;
        }
        // Only the researched cloud forecast kinds get a region-aware rank;
        // everything else keeps the priority its own add path chose.
        if !entry.source.is_forecast() {
            continue;
        }
        entry.priority = default_priority_for(&entry.source, lat, lon);
        entry.enabled = entry.enabled && default_enabled_for(&entry.source, lat, lon);
        // Seed the freshness window for a hand-added (UI / raw-TOML) cloud source
        // that came in with `max_age_s = None`. Without this a slow-cadence kind
        // (Open-Meteo, NWS, Met.no) falls back to LIVE_FRESHNESS_SECS in the merge
        // and a per-field pin to it expires mid-cadence; the wizard add path
        // already seeds this, so this closes the same gap on the raw-add path.
        // The keyed providers (OpenWeather, Pirate, WeatherKit) now seed the 3900s
        // health-bucket window too; only kinds with no region default keep None.
        if entry.max_age_s.is_none() {
            entry.max_age_s = default_max_age_for(&entry.source);
        }
    }
}

/// The default-on keyless regional authority source(s) for a no-hardware
/// install at `lat`/`lon`, ready to push onto a fresh (empty) source list.
///
/// A no-hardware user boots with the region's KEYLESS authority live, zero
/// clicks: NWS inside the US bbox, Met.no inside the Europe/Nordic bbox, and
/// nothing extra elsewhere (Global keeps Open-Meteo as the sole keyless cloud,
/// since no other keyless regional authority covers it). Both NWS and Met.no
/// are keyless: their one required field is a `user_agent`, which we auto-fill
/// with `KEYLESS_AUTHORITY_USER_AGENT` exactly as the existing default sources
/// do (an empty one would fail config validation).
///
/// Each entry's rank, freshness window, and enablement are seeded from the same
/// `default_priority_for` / `default_max_age_for` / `default_enabled_for`
/// helpers the wizard/env_compat Open-Meteo seeding uses, so a US NWS lands at
/// priority 70 with the 2100s slow-cadence window enabled, and a Nordic Met.no
/// lands at 70 enabled, both ABOVE the always-on Open-Meteo backstop (50). A
/// real live LAN station, added later, still outranks them (it sits at 100 and
/// is `live_current = true`); these cloud authorities are `live_current = false`.
///
/// This NEVER returns a keyed source (Pirate / OpenWeather / WeatherKit): those
/// require an operator-supplied key and must stay opt-in.
///
/// Caller contract: only invoke this on an EMPTY source list (a fresh,
/// no-hardware install). The id is stable (`nws` / `met_norway`) so a later
/// `normalize_new_cloud_sources` pass is id-keyed and idempotent over it.
pub fn region_keyless_authority_entries(
    lat: f64,
    lon: f64,
) -> Vec<crate::config::schema::SourceEntry> {
    use crate::config::schema::SourceEntry;

    // Single source of truth: derive the set from `region_keyless_authority_kinds`
    // (which itself agrees with `is_region_keyless_authority`), then drop
    // Open-Meteo, the always-on member the caller seeds separately, so this
    // returns only the EXTRA regional authority (NWS in the US, Met.no in the
    // Nordics, nothing in Global). This keeps the persistable-entries builder and
    // the recommendation predicate provably in lockstep. A stable id (`nws` /
    // `met_norway`) keeps a later id-keyed `normalize_new_cloud_sources` pass
    // idempotent. Both ids resolve a keyless source whose one required field is a
    // `user_agent`, auto-filled with `KEYLESS_AUTHORITY_USER_AGENT` so it passes
    // the config validator.
    region_keyless_authority_kinds(lat, lon)
        .into_iter()
        .filter(|kind| !matches!(kind, SourceKind::OpenMeteo(_)))
        .map(|kind| {
            let id = match &kind {
                SourceKind::Nws(_) => "nws",
                SourceKind::NoaaMrms(_) => "noaa_mrms",
                SourceKind::MetNorway(_) => "met_norway",
                // The kinds list only ever adds NWS / NOAA MRMS / Met.no as the
                // extra authority; any other variant here would be a bug in
                // `region_keyless_authority_kinds`, so fall back to the kind label.
                other => crate::config::kind_labels::source_kind_label(other),
            };
            SourceEntry {
                id: id.to_string(),
                priority: default_priority_for(&kind, lat, lon),
                max_age_s: default_max_age_for(&kind),
                enabled: default_enabled_for(&kind, lat, lon),
                source: kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{
        MetNorwayConfig, NwsConfig, OpenWeatherConfig, PirateWeatherConfig,
    };

    // Representative points.
    const ORLANDO: (f64, f64) = (28.5, -81.4); // US (CONUS)
    const OSLO: (f64, f64) = (59.9, 10.75); // Europe/Nordic
    const SYDNEY: (f64, f64) = (-33.87, 151.2); // Global (non-US, non-Europe)
    const ANCHORAGE: (f64, f64) = (61.2, -149.9); // US (Alaska)
    const HONOLULU: (f64, f64) = (21.3, -157.85); // US (Hawaii)

    // OpenMeteoConfig is fully serde-defaulted (round-trip an empty object, as
    // the wizard does); the keyed/agent configs carry required fields, so build
    // those directly with struct literals.
    fn om() -> SourceKind {
        SourceKind::OpenMeteo(serde_json::from_value(serde_json::json!({})).unwrap())
    }
    fn nws() -> SourceKind {
        SourceKind::Nws(NwsConfig {
            user_agent: "LocalSky (test)".into(),
        })
    }
    fn mrms() -> SourceKind {
        SourceKind::NoaaMrms(crate::config::schema::NoaaMrmsConfig::default())
    }
    fn pirate() -> SourceKind {
        SourceKind::PirateWeather(PirateWeatherConfig {
            api_key: "x".into(),
        })
    }
    fn metno() -> SourceKind {
        SourceKind::MetNorway(MetNorwayConfig {
            user_agent: "LocalSky (test)".into(),
        })
    }
    fn ow() -> SourceKind {
        SourceKind::OpenWeather(OpenWeatherConfig {
            api_key: "x".into(),
        })
    }

    #[test]
    fn region_resolution() {
        assert_eq!(region_for(ORLANDO.0, ORLANDO.1), Region::Us);
        assert_eq!(region_for(ANCHORAGE.0, ANCHORAGE.1), Region::Us);
        assert_eq!(region_for(HONOLULU.0, HONOLULU.1), Region::Us);
        assert_eq!(region_for(OSLO.0, OSLO.1), Region::EuropeNordic);
        assert_eq!(region_for(SYDNEY.0, SYDNEY.1), Region::Global);
    }

    #[test]
    fn us_point_ranks_nws_over_pirate_over_open_meteo() {
        let (lat, lon) = ORLANDO;
        let p_nws = default_priority_for(&nws(), lat, lon);
        let p_pirate = default_priority_for(&pirate(), lat, lon);
        let p_om = default_priority_for(&om(), lat, lon);
        assert!(
            p_nws > p_pirate && p_pirate > p_om,
            "US ranking must be NWS({p_nws}) > Pirate({p_pirate}) > Open-Meteo({p_om})"
        );
        // NWS is enabled in the US.
        assert!(default_enabled_for(&nws(), lat, lon));
    }

    #[test]
    fn nordic_point_ranks_metno_highest() {
        let (lat, lon) = OSLO;
        let p_metno = default_priority_for(&metno(), lat, lon);
        let p_ow = default_priority_for(&ow(), lat, lon);
        let p_om = default_priority_for(&om(), lat, lon);
        assert!(
            p_metno > p_ow && p_metno > p_om,
            "Nordic ranking must put Met.no({p_metno}) above OpenWeather({p_ow}) and Open-Meteo({p_om})"
        );
    }

    #[test]
    fn nws_disabled_outside_us() {
        // Non-US points must not enable NWS by default (US-only coverage).
        assert!(!default_enabled_for(&nws(), SYDNEY.0, SYDNEY.1));
        assert!(!default_enabled_for(&nws(), OSLO.0, OSLO.1));
        // And it is enabled inside the US.
        assert!(default_enabled_for(&nws(), ORLANDO.0, ORLANDO.1));
    }

    #[test]
    fn open_meteo_is_always_the_backstop_50() {
        for (lat, lon) in [ORLANDO, OSLO, SYDNEY, ANCHORAGE, HONOLULU] {
            assert_eq!(
                default_priority_for(&om(), lat, lon),
                50,
                "Open-Meteo must always be the 50-priority keyless backstop"
            );
        }
    }

    use crate::config::schema::{Config, SourceEntry};

    // Build a Config at a given location with a given source list.
    fn cfg_at(lat: f64, lon: f64, sources: Vec<SourceEntry>) -> Config {
        let mut c = Config::default();
        c.deployment.location.lat = lat;
        c.deployment.location.lon = lon;
        c.sources = sources;
        c
    }

    // A SourceEntry with an explicit flat priority + enabled, as the UI add path
    // produces it (priority 50, enabled true) before normalization.
    fn entry(id: &str, kind: SourceKind, priority: i32, enabled: bool) -> SourceEntry {
        SourceEntry {
            id: id.to_string(),
            priority,
            enabled,
            max_age_s: None,
            source: kind,
        }
    }

    #[test]
    fn newly_added_nws_in_us_gets_70_open_meteo_stays_50() {
        let (lat, lon) = ORLANDO;
        // Previous config already has Open-Meteo; the user adds NWS this write.
        // Both arrive as the UI's flat priority 50.
        let prev = cfg_at(lat, lon, vec![entry("open_meteo", om(), 50, true)]);
        let mut next = cfg_at(
            lat,
            lon,
            vec![
                entry("open_meteo", om(), 50, true),
                entry("nws", nws(), 50, true),
            ],
        );
        normalize_new_cloud_sources(Some(&prev), &mut next);

        let nws_e = next.sources.iter().find(|s| s.id == "nws").unwrap();
        let om_e = next.sources.iter().find(|s| s.id == "open_meteo").unwrap();
        // The newly-added NWS is lifted to its US rank (70), no longer tying OM.
        assert_eq!(nws_e.priority, 70, "added NWS must take the US rank 70");
        assert!(nws_e.enabled, "NWS is enabled in the US");
        // Open-Meteo existed before, so it is untouched (and 50 either way).
        assert_eq!(om_e.priority, 50, "Open-Meteo stays the 50 backstop");
        // The two clouds no longer tie, so the merge has a deterministic owner.
        assert!(nws_e.priority > om_e.priority);
    }

    #[test]
    fn newly_added_nws_outside_us_is_disabled() {
        let (lat, lon) = SYDNEY;
        // Fresh install (prev = None): the user adds NWS at a non-US location.
        let mut next = cfg_at(lat, lon, vec![entry("nws", nws(), 50, true)]);
        normalize_new_cloud_sources(None, &mut next);

        let nws_e = next.sources.iter().find(|s| s.id == "nws").unwrap();
        // NWS has no coverage outside the US, so it is disabled (no permanent
        // 404 / degraded banner), and ranked below any keyed provider.
        assert!(
            !nws_e.enabled,
            "NWS added outside the US must be auto-disabled"
        );
        assert_eq!(
            nws_e.priority, 30,
            "out-of-US NWS sits below keyed providers"
        );
    }

    #[test]
    fn existing_customized_cloud_priority_is_preserved() {
        let (lat, lon) = ORLANDO;
        // The user previously added NWS and hand-tuned it down to 42, enabled.
        let prev = cfg_at(lat, lon, vec![entry("nws", nws(), 42, true)]);
        // A later save (e.g. they toggled some unrelated setting) re-PUTs the
        // same NWS source. It is NOT new, so normalize must not re-clamp it.
        let mut next = cfg_at(lat, lon, vec![entry("nws", nws(), 42, true)]);
        normalize_new_cloud_sources(Some(&prev), &mut next);

        let nws_e = next.sources.iter().find(|s| s.id == "nws").unwrap();
        assert_eq!(
            nws_e.priority, 42,
            "an existing source's hand-set priority survives a re-save"
        );
        assert!(nws_e.enabled, "and its enablement is left as-is");
    }

    #[test]
    fn user_disabled_new_cloud_stays_disabled() {
        let (lat, lon) = ORLANDO;
        // A user explicitly adds NWS but leaves it DISABLED. The region gate
        // (enabled in the US) must not silently flip a user's off back on.
        let mut next = cfg_at(lat, lon, vec![entry("nws", nws(), 50, false)]);
        normalize_new_cloud_sources(None, &mut next);

        let nws_e = next.sources.iter().find(|s| s.id == "nws").unwrap();
        assert!(
            !nws_e.enabled,
            "enabled is AND-ed: a user-disabled add stays disabled"
        );
        // Priority is still seeded to the region rank.
        assert_eq!(nws_e.priority, 70);
    }

    #[test]
    fn newly_added_slow_cadence_cloud_gets_widened_max_age() {
        let (lat, lon) = ORLANDO;
        // A hand-added (UI / raw-TOML) cloud source arrives with max_age_s = None
        // (the `entry` helper mirrors that add path exactly). Normalize must seed
        // the widened window for the slow-cadence kinds so a per-field pin to one
        // does not expire mid-refresh-cycle; the keyed 600s-cadence kinds keep
        // None (their own kind-default window).
        let mut next = cfg_at(
            lat,
            lon,
            vec![
                entry("nws", nws(), 50, true),
                entry("met_norway", metno(), 50, true),
                entry("open_meteo_2", om(), 50, true),
                entry("openweather", ow(), 50, true),
                entry("pirate_weather", pirate(), 50, true),
            ],
        );
        normalize_new_cloud_sources(None, &mut next);

        let max_age = |id: &str| next.sources.iter().find(|s| s.id == id).unwrap().max_age_s;
        // Slow-cadence kinds get the widened 2100s window.
        assert_eq!(max_age("nws"), Some(MAX_AGE_SLOW_CADENCE_S));
        assert_eq!(max_age("met_norway"), Some(MAX_AGE_SLOW_CADENCE_S));
        assert_eq!(max_age("open_meteo_2"), Some(MAX_AGE_SLOW_CADENCE_S));
        assert_eq!(MAX_AGE_SLOW_CADENCE_S, 2100);
        // The keyed current-conditions kinds now seed the 3900s health-bucket
        // window so a normal poll gap does not read stale and flap the rain owner.
        assert_eq!(max_age("openweather"), Some(3900));
        assert_eq!(max_age("pirate_weather"), Some(3900));
    }

    #[test]
    fn existing_cloud_source_max_age_is_preserved() {
        let (lat, lon) = ORLANDO;
        // An existing Open-Meteo the user hand-set to a 900s window. A re-save is
        // not a new source, so normalize must not touch its max_age_s (the loop is
        // id-keyed and skips it), leaving the explicit pin intact.
        let mut prev_om = entry("open_meteo", om(), 50, true);
        prev_om.max_age_s = Some(900);
        let prev = cfg_at(lat, lon, vec![prev_om.clone()]);
        let mut next = cfg_at(lat, lon, vec![prev_om]);
        normalize_new_cloud_sources(Some(&prev), &mut next);

        let om_e = next.sources.iter().find(|s| s.id == "open_meteo").unwrap();
        assert_eq!(
            om_e.max_age_s,
            Some(900),
            "an existing source's hand-set max_age_s survives a re-save"
        );
    }

    #[test]
    fn non_cloud_source_priority_is_not_clobbered() {
        let (lat, lon) = ORLANDO;
        // A LAN station added this write at its proper 100. It is NOT a cloud
        // forecast kind, so normalize must leave its priority alone (the region
        // helper's `_ => 50` fallthrough would wrongly drop it to 50).
        let station = SourceEntry {
            id: "tempest_lan".to_string(),
            priority: 100,
            enabled: true,
            max_age_s: None,
            source: SourceKind::TempestUdp(serde_json::from_value(serde_json::json!({})).unwrap()),
        };
        let mut next = cfg_at(lat, lon, vec![station]);
        normalize_new_cloud_sources(None, &mut next);

        let s = next.sources.iter().find(|s| s.id == "tempest_lan").unwrap();
        assert_eq!(
            s.priority, 100,
            "a non-forecast local station keeps its own add-path priority"
        );
    }

    #[test]
    fn slow_cadence_sources_get_widened_max_age() {
        // The ~1800s-cadence forecast kinds get the widened window so a pin
        // survives the refresh cycle; everything else keeps the kind default.
        assert_eq!(default_max_age_for(&om()), Some(MAX_AGE_SLOW_CADENCE_S));
        assert_eq!(default_max_age_for(&nws()), Some(MAX_AGE_SLOW_CADENCE_S));
        assert_eq!(default_max_age_for(&metno()), Some(MAX_AGE_SLOW_CADENCE_S));
        // NOAA MRMS gets the WIDER 2 hr window (not the 2100s slow-cadence one):
        // its gauge-corrected hourly accumulation field is inherently ~1 to 1.5 hr
        // behind, so a 35 min window would always read it stale in the merge.
        assert_eq!(default_max_age_for(&mrms()), Some(MAX_AGE_MRMS_S));
        assert_eq!(MAX_AGE_MRMS_S, 7200);
        // MRMS window must exceed the slow-cadence window so the lagged hourly
        // accumulation stays fresh (a const comparison, so const-asserted).
        const _: () = assert!(MAX_AGE_MRMS_S > MAX_AGE_SLOW_CADENCE_S);
        // The keyed current-conditions providers (OpenWeather, Pirate, WeatherKit)
        // get the 3900s window matching the /api/health kind bucket, so a normal
        // ~10 to 60 min poll gap is not judged stale (which would lose their fill
        // or flap the rain owner). They are no longer None.
        assert_eq!(default_max_age_for(&ow()), Some(3900));
        assert_eq!(default_max_age_for(&pirate()), Some(3900));
        // The MRMS rate gets its own tight per-field window, separate from the
        // wide accumulation window (used by max_age_for_field in the merge).
        assert_eq!(MAX_AGE_MRMS_RATE_S, 900);
        const { assert!(MAX_AGE_MRMS_RATE_S < MAX_AGE_MRMS_S) };
    }

    #[test]
    fn us_keyless_authority_is_nws_at_70_enabled() {
        // A no-hardware US install gets NWS + NOAA MRMS live, zero clicks: NWS at
        // the US station-authority rank (70) with the 2100s slow-cadence window
        // and a non-empty auto-filled user_agent (an empty one would fail the
        // config validator), and NOAA MRMS at 75 (above NWS, below a LAN gauge).
        // Both above the Open-Meteo backstop (50).
        for (lat, lon) in [ORLANDO, ANCHORAGE, HONOLULU] {
            let entries = region_keyless_authority_entries(lat, lon);
            assert_eq!(
                entries.len(),
                2,
                "US synthesizes the NWS + NOAA MRMS authorities"
            );

            let nws_e = entries.iter().find(|e| e.id == "nws").expect("NWS seeded");
            assert_eq!(nws_e.priority, 70, "US NWS lands at the authority rank 70");
            assert!(nws_e.enabled, "NWS is enabled in the US, zero clicks");
            assert_eq!(
                nws_e.max_age_s,
                Some(MAX_AGE_SLOW_CADENCE_S),
                "NWS gets the 2100s slow-cadence freshness window"
            );
            match &nws_e.source {
                SourceKind::Nws(c) => assert!(
                    !c.user_agent.trim().is_empty(),
                    "the keyless NWS user_agent must be auto-filled (empty fails validation)"
                ),
                other => panic!("expected an NWS source, got {other:?}"),
            }

            let mrms_e = entries
                .iter()
                .find(|e| e.id == "noaa_mrms")
                .expect("NOAA MRMS seeded");
            assert_eq!(
                mrms_e.priority, 75,
                "US NOAA MRMS lands just above NWS (75)"
            );
            assert!(
                mrms_e.enabled,
                "NOAA MRMS is enabled in the US, zero clicks"
            );
            assert_eq!(
                mrms_e.max_age_s,
                Some(MAX_AGE_MRMS_S),
                "NOAA MRMS gets the wider 2 hr window so its lagged hourly accumulation field stays fresh"
            );
            assert!(
                mrms_e.priority > nws_e.priority,
                "MRMS radar QPE outranks the NWS station observation"
            );
            assert!(
                matches!(mrms_e.source, SourceKind::NoaaMrms(_)),
                "expected a NoaaMrms source"
            );
        }
    }

    #[test]
    fn nordic_keyless_authority_is_met_norway_at_70_enabled() {
        // A no-hardware Nordic install gets Met.no live, zero clicks.
        let entries = region_keyless_authority_entries(OSLO.0, OSLO.1);
        assert_eq!(
            entries.len(),
            1,
            "Nordic synthesizes exactly the Met.no authority"
        );
        let metno_e = &entries[0];
        assert_eq!(metno_e.id, "met_norway");
        assert_eq!(
            metno_e.priority, 70,
            "Nordic Met.no lands at the authority rank 70"
        );
        assert!(
            metno_e.enabled,
            "Met.no is enabled in the Nordics, zero clicks"
        );
        assert_eq!(metno_e.max_age_s, Some(MAX_AGE_SLOW_CADENCE_S));
        match &metno_e.source {
            SourceKind::MetNorway(c) => assert!(
                !c.user_agent.trim().is_empty(),
                "the keyless Met.no user_agent must be auto-filled"
            ),
            other => panic!("expected a MetNorway source, got {other:?}"),
        }
    }

    #[test]
    fn keyless_authority_predicate_matches_seeded_set() {
        // The predicate must return exactly the set finalize_sources/synthesize
        // auto-enable: Open-Meteo everywhere, NWS in the US, Met.no in the
        // Nordics, nothing else. A paid provider is never recommended, and an
        // out-of-region keyless kind is not recommended there.

        // US point: Open-Meteo + NWS + NOAA MRMS true; Met.no + keyed false.
        let (lat, lon) = ORLANDO;
        assert!(is_region_keyless_authority(&om(), lat, lon));
        assert!(is_region_keyless_authority(&nws(), lat, lon));
        assert!(is_region_keyless_authority(&mrms(), lat, lon));
        assert!(!is_region_keyless_authority(&metno(), lat, lon));
        assert!(!is_region_keyless_authority(&ow(), lat, lon));
        assert!(!is_region_keyless_authority(&pirate(), lat, lon));

        // Nordic point: Open-Meteo + Met.no true; NWS + MRMS + keyed false.
        let (lat, lon) = OSLO;
        assert!(is_region_keyless_authority(&om(), lat, lon));
        assert!(is_region_keyless_authority(&metno(), lat, lon));
        assert!(!is_region_keyless_authority(&nws(), lat, lon));
        assert!(!is_region_keyless_authority(&mrms(), lat, lon));
        assert!(!is_region_keyless_authority(&ow(), lat, lon));

        // Global point: only Open-Meteo true.
        let (lat, lon) = SYDNEY;
        assert!(is_region_keyless_authority(&om(), lat, lon));
        assert!(!is_region_keyless_authority(&nws(), lat, lon));
        assert!(!is_region_keyless_authority(&mrms(), lat, lon));
        assert!(!is_region_keyless_authority(&metno(), lat, lon));
        assert!(!is_region_keyless_authority(&ow(), lat, lon));
    }

    #[test]
    fn is_region_appropriate_collapses_metno_outside_nordics() {
        // Met.no is only region-appropriate in the Nordics; everything else is
        // appropriate everywhere (the softer UI-collapse signal vs enablement).
        assert!(is_region_appropriate(&metno(), OSLO.0, OSLO.1));
        assert!(!is_region_appropriate(&metno(), ORLANDO.0, ORLANDO.1));
        assert!(!is_region_appropriate(&metno(), SYDNEY.0, SYDNEY.1));
        // NWS / NOAA MRMS / Open-Meteo are region-appropriate everywhere here
        // (US-only coverage is gated by default_enabled_for, not this predicate).
        for (lat, lon) in [ORLANDO, OSLO, SYDNEY] {
            assert!(is_region_appropriate(&nws(), lat, lon));
            assert!(is_region_appropriate(&mrms(), lat, lon));
            assert!(is_region_appropriate(&om(), lat, lon));
        }
    }

    #[test]
    fn keyless_authority_kinds_lists_the_seeded_set() {
        // Open-Meteo + NWS + NOAA MRMS in the US.
        let us = region_keyless_authority_kinds(ORLANDO.0, ORLANDO.1);
        assert!(us.iter().any(|k| matches!(k, SourceKind::OpenMeteo(_))));
        assert!(us.iter().any(|k| matches!(k, SourceKind::Nws(_))));
        assert!(us.iter().any(|k| matches!(k, SourceKind::NoaaMrms(_))));
        assert!(!us.iter().any(|k| matches!(k, SourceKind::MetNorway(_))));
        assert_eq!(us.len(), 3);

        // Open-Meteo + Met.no in the Nordics.
        let nordic = region_keyless_authority_kinds(OSLO.0, OSLO.1);
        assert!(nordic.iter().any(|k| matches!(k, SourceKind::OpenMeteo(_))));
        assert!(nordic.iter().any(|k| matches!(k, SourceKind::MetNorway(_))));
        assert_eq!(nordic.len(), 2);

        // Open-Meteo only in Global.
        let global = region_keyless_authority_kinds(SYDNEY.0, SYDNEY.1);
        assert_eq!(global.len(), 1);
        assert!(matches!(global[0], SourceKind::OpenMeteo(_)));

        // The predicate and the enumerated set agree everywhere.
        for (lat, lon) in [ORLANDO, OSLO, SYDNEY, ANCHORAGE, HONOLULU] {
            for kind in region_keyless_authority_kinds(lat, lon) {
                assert!(
                    is_region_keyless_authority(&kind, lat, lon),
                    "an enumerated authority kind must satisfy the predicate"
                );
            }
        }
    }

    #[test]
    fn global_has_no_keyless_regional_authority() {
        // Outside the US and Europe/Nordic boxes no keyless regional authority
        // covers the location, so the caller's Open-Meteo is the only keyless
        // cloud; we never auto-enable a keyed provider here.
        assert!(
            region_keyless_authority_entries(SYDNEY.0, SYDNEY.1).is_empty(),
            "a non-US/Nordic install synthesizes no extra keyless authority"
        );
    }
}
