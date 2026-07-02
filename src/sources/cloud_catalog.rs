// Honest cloud-source catalog: the single source of truth for the per-service
// facts the no-hardware "cloud weather" experience renders against.
//
// WHY THIS EXISTS. Cloud sources are never a live LAN station (a real station
// always outranks them: capabilities().live_current=false for every kind here).
// So the UI must be HONEST about what each cloud service actually is: a real
// station report, a radar nowcast, or a model forecast. This module derives
// that honesty metadata from the cloud-source audit, in plain English with no
// jargon, so the cloud-onboarding UI (Wave B) can describe each option, mark
// the regionally recommended one, gray out NWS outside the US, and never imply
// a forecast is a measurement.
//
// SCOPE. The cloud weather services carry catalog metadata: NWS, NOAA MRMS,
// Pirate Weather, OpenWeather, Apple WeatherKit, Open-Meteo, Met.no, plus the
// CLOUD WEATHER STATION tier (Ambient Weather, Netatmo, LaCrosse). Most of the
// first group are the CLOUD FORECAST kinds (`SourceKind::is_forecast`); NOAA
// MRMS is the lone observation-grade radar-QPE member (not a forecast provider)
// but is still a keyless cloud rain SERVICE, so it belongs in this honest
// catalog. The cloud weather STATIONS are a real consumer station the user owns,
// reached through the vendor cloud (`live_current=true` adapters): an honest
// `Observation` tier, distinct from both the forecast clouds and a direct-LAN
// station. A direct-LAN station, sensor gateways, HA passthrough, demo, and the
// generic mapping sources are not cloud weather services and are surfaced
// elsewhere (the per-field + forecast pickers in /api/config).
//
// PURE. Every function here is a deterministic map of (kind) -> facts (plus
// (kind, lat, lon) for the region recommendation, delegated to `config::region`
// so the ranking stays single-sourced). No env, no I/O, unit-testable in
// isolation. The runtime field list + already_configured wiring lives in the
// /api/config/source_catalog handler that consumes this; this module owns only
// the static honest facts.

use serde::Serialize;

use crate::config::schema::{
    AmbientWeatherConfig, LacrosseConfig, MetNorwayConfig, NetatmoConfig, NoaaMrmsConfig,
    NwsConfig, OpenMeteoConfig, OpenWeatherConfig, PirateWeatherConfig, SourceKind, SynopticConfig,
    WeatherKitConfig,
};

/// What a cloud reading FUNDAMENTALLY is, the honesty axis the UI leads with:
///
///   * `Observation`: a real instrument report from a physical station (NWS).
///     It can lag, and it is not your yard, but it is a measurement.
///   * `RadarQpe`: a gauge-corrected radar rain estimate (NOAA MRMS). It is
///     observation-grade, NOT a model forecast: it measures the rain that
///     actually fell on a 1 km cell over your block, the best off-yard rain
///     read short of your own gauge.
///   * `Nowcast`: a very-short-range analysis blending live radar + station
///     reports (Pirate Weather in the US/Canada). Seconds of lag, a grid
///     estimate, the best live-rain proxy short of your own gauge.
///   * `Forecast`: a model or ML estimate for the current interval, never a
///     direct measurement (Open-Meteo, Met.no, OpenWeather, WeatherKit, and
///     Pirate's rain, which is HRRR/GEFS model output, not radar).
///
/// Serializes snake_case (`observation` / `radar_qpe` / `nowcast` / `forecast`)
/// so the UI matches on a stable string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudDataNature {
    Observation,
    RadarQpe,
    Nowcast,
    Forecast,
}

/// What the user must bring to use a cloud service, the cost/friction axis:
///
///   * `NoKey`: works with no account and no key (NWS, Open-Meteo, Met.no).
///   * `FreeKey`: a free API key, no payment (Pirate Weather).
///   * `Paid`: a paid plan or a paid developer account (OpenWeather card on
///     file, Apple WeatherKit at 99 dollars a year).
///
/// Serializes snake_case (`no_key` / `free_key` / `paid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyTier {
    NoKey,
    FreeKey,
    Paid,
}

/// The honest per-service facts for one cloud weather kind, derived from the
/// audit. Every string is the EXACT plain-English audit copy (no jargon, no em
/// dashes); the UI renders these verbatim. Static: no runtime state, no config.
#[derive(Debug, Clone, Serialize)]
pub struct CloudSourceMeta {
    /// Stable kind tag (matches `kind_labels::source_kind_label`), the key the
    /// UI joins this metadata to a configured source / picker option on.
    pub kind: &'static str,
    /// What this reading fundamentally is (the headline honesty axis).
    pub data_nature: CloudDataNature,
    /// The HONEST nature of the CURRENT-RAIN signal specifically, which can
    /// differ from the entry's overall `data_nature`. This is what the honest
    /// RAIN BADGE is derived from (not `data_nature`): it is the mislabel fix for
    /// Pirate, whose rain is HRRR/GEFS model output (`Forecast`) even though its
    /// overall headline is `Nowcast` for temp/wind. NWS rain is `Observation`,
    /// NOAA MRMS rain is `RadarQpe`, and every model provider's rain is
    /// `Forecast`. The UI maps Observation|RadarQpe -> green "measures rain",
    /// Nowcast -> blue, Forecast -> amber "forecast only", and NEVER the word
    /// "live" on a Forecast.
    pub rain_nature: CloudDataNature,
    /// One plain line on how live + how laggy "current" really is.
    pub real_time: &'static str,
    /// One plain line on how close to the user's yard the value resolves.
    pub localization: &'static str,
    /// One honest line on the watering-decision risk of trusting its rain.
    pub watering_risk: &'static str,
    /// What the user must bring (no key / free key / paid).
    pub key_tier: KeyTier,
    /// POST-FIX truth: does the adapter emit a CURRENT rain scalar into the
    /// merge? True for every kind after the adapter fixes EXCEPT Met.no, which
    /// has no current observation. The UI uses this to promise (or withhold)
    /// a live "is it raining now" answer from a given cloud option.
    pub emits_current_rain: bool,
    /// True ONLY for Met.no: its probability-of-precipitation is a fabricated
    /// (synthesized) value, not a modeled POP, so the UI must flag it rather
    /// than present it as a real forecast probability.
    pub pop_is_synthetic: bool,
    /// Honesty ranking for default ordering, HIGHEST first. NWS (a real
    /// observation) > Pirate (radar nowcast) > OpenWeather > WeatherKit >
    /// Open-Meteo (keyless model) > Met.no (coarse model + synthetic POP). The
    /// UI sorts the cloud options by this so the most honest option leads.
    pub honesty_rank: i32,
    /// Best-rain-DECISION ranking for irrigation, HIGHEST first. This is the
    /// "whose rain do I trust to decide watering" axis, distinct from
    /// `honesty_rank` (which is a presentation-order axis). The order is: a
    /// gauge on your yard highest (ranked elsewhere, ~100), then NOAA MRMS
    /// (radar QPE), then NWS (station observation), then Pirate, WeatherKit,
    /// OpenWeather, Open-Meteo, and Met.no lowest. The engine + UI use this to
    /// pick the most trustworthy rain signal available for the skip decision.
    pub irrigation_rank: i32,
    /// A one-line "why you might still want this" upgrade note, `Some` only for
    /// Pirate in CONUS: even though its RAIN is a model forecast (the mislabel
    /// fix), its free-key nowcast still upgrades the temp/wind current reads.
    /// The UI shows this so a CONUS user understands Pirate's honest value
    /// without being misled that its rain is measured. `None` for every other
    /// kind.
    pub upgrade_reason: Option<&'static str>,
}

/// The canonical merge keys for the rain fields, the ONLY fields whose nature is
/// driven by the per-row `rain_nature` override rather than the overall
/// `data_nature`. Kept as one const so `field_nature` and any future caller agree
/// on exactly which keys are "rain": the today total, the instantaneous rate, and
/// the probability-of-precipitation. These are the `field_overrides::field_name`
/// snake_case keys (`RainTodayIn` / `RainIntensityInHr` / `Pop`), the SAME keys
/// `runtime::source_field_names` emits into `live_current_fields`, so the Panel
/// joins a nature to a lit cell with no key drift.
const RAIN_FIELD_KEYS: &[&str] = &["rain_today_in", "rain_intensity_in_hr", "pop"];

impl CloudSourceMeta {
    /// The HONEST per-field data nature for one canonical merge key (the
    /// snake_case `field_overrides::field_name` of a `WeatherField`, e.g.
    /// `"wind_mph"`, `"rain_today_in"`). This is the per-CELL truth the capability
    /// matrix renders: the single overall `data_nature` cannot say "this kind's
    /// wind is a live nowcast while its rain is a model forecast", so the Panel
    /// asks per field.
    ///
    /// THE RULE, by construction:
    ///   * The RAIN fields (`RAIN_FIELD_KEYS`: today total, rate, POP) inherit the
    ///     per-row `rain_nature` override (the mislabel fix: Pirate's rain is a
    ///     `Forecast` even though its headline is `Nowcast`).
    ///   * Every OTHER field inherits the overall `data_nature` UNLESS this kind
    ///     carries an explicit per-field override below.
    ///
    /// PER-KIND OVERRIDES (the CONTRACT). Keyed on `self.kind` so the map lives
    /// next to the rest of the kind's honest facts and a new kind is a one-line
    /// add. Today only Open-Meteo needs a non-rain override: its native ET0
    /// (`et0_today`) is a MODELED daily total, a `Forecast`, even though its
    /// headline `data_nature` is already `Forecast` (so the override is a no-op
    /// today but documents intent and guards a future headline change). Pirate,
    /// OpenWeather, WeatherKit, NWS, MRMS, Met.no and the PWS station tier need no
    /// non-rain override: their every non-rain field IS their headline nature
    /// (Pirate temp/wind/pressure/UV = `Nowcast`; PWS temp/wind/pressure = the
    /// station `Observation`; OpenWeather/WeatherKit/Open-Meteo non-rain = model
    /// `Forecast`), so they fall through to `data_nature`.
    ///
    /// Total over every input: an unknown / not-emitted key still resolves (to the
    /// rain override if it is a rain key, else `data_nature`), so the caller can
    /// ask for any canonical key without a panic.
    pub fn field_nature(&self, field: &str) -> CloudDataNature {
        // Rain fields always track the honest rain override, never the headline.
        if RAIN_FIELD_KEYS.contains(&field) {
            return self.rain_nature;
        }
        // Per-kind NON-rain overrides (the CONTRACT). Open-Meteo's ET0 is a
        // modeled daily total (a forecast), called out explicitly so it stays
        // honest even if the headline data_nature ever changes.
        if self.kind == "open_meteo" && field == "et0_today" {
            return CloudDataNature::Forecast;
        }
        // Everything else IS this kind's headline nature: Pirate temp/wind/
        // pressure/UV = Nowcast, the PWS tier temp/wind/pressure = Observation,
        // the model providers' non-rain fields = Forecast, NWS/MRMS = their obs
        // natures.
        self.data_nature
    }
}

/// The cloud weather kinds, each paired with a representative `SourceKind`
/// instance. The instances exist only to drive the variant-keyed lookups in
/// `config::region` (which match the ENUM variant, never the config payload)
/// and `runtime::source_field_names` (which builds an adapter to read its
/// capability set); the placeholder key/agent strings are never used for a real
/// request. Ordered HIGHEST honesty first so a caller that iterates renders the
/// most honest option first by default: NWS (an official station observation),
/// then the cloud weather STATION tier (a real station the user owns, cloud
/// routed: Ambient Weather, Netatmo, LaCrosse), then radar QPE, then the
/// nowcast/model tiers.
pub fn cloud_kinds() -> Vec<SourceKind> {
    vec![
        SourceKind::Nws(NwsConfig {
            user_agent: "LocalSky (catalog)".to_string(),
        }),
        // The CLOUD WEATHER STATION tier: a real consumer station the user owns,
        // reached through the vendor cloud. Already wired adapters
        // (`ambient_weather` / `netatmo` / `lacrosse`, `live_current=true` at
        // ~priority 70) that were absent from the honest catalog and so invisible
        // in the cloud panel. Surfaced here as a distinct OBSERVATION tier so they
        // earn a matrix row + recommendation treatment. Placeholder configs: the
        // VARIANT is all the variant-keyed lookups read.
        SourceKind::AmbientWeather(AmbientWeatherConfig {
            app_key: String::new(),
            api_key: String::new(),
            mac_address: String::new(),
        }),
        SourceKind::Netatmo(NetatmoConfig {
            client_id: String::new(),
            client_secret: String::new(),
            refresh_token: String::new(),
            device_id: String::new(),
        }),
        SourceKind::Lacrosse(LacrosseConfig {
            email: String::new(),
            password: String::new(),
            device_id: None,
        }),
        // Synoptic (MesoWest): a real mesonet station observation via a free
        // token. Sits just under the cloud weather STATION tier (your OWN
        // station) since it is someone else's station, above the radar QPE.
        SourceKind::Synoptic(SynopticConfig {
            token: String::new(),
            station_id: None,
            radius_mi: 25.0,
        }),
        SourceKind::NoaaMrms(NoaaMrmsConfig::default()),
        SourceKind::PirateWeather(PirateWeatherConfig {
            api_key: String::new(),
        }),
        SourceKind::OpenWeather(OpenWeatherConfig {
            api_key: String::new(),
        }),
        SourceKind::WeatherKit(WeatherKitConfig {
            key_id: String::new(),
            team_id: String::new(),
            service_id: String::new(),
            private_key_pem: String::new(),
            language: "en".to_string(),
        }),
        SourceKind::OpenMeteo(OpenMeteoConfig {
            forecast_days: 7,
            forecast_hours: 48,
            past_days: 1,
            include_radar: false,
            model: "best_match".to_string(),
        }),
        SourceKind::MetNorway(MetNorwayConfig {
            user_agent: "LocalSky (catalog)".to_string(),
        }),
    ]
}

/// The honest catalog facts for a cloud forecast kind, or `None` for any kind
/// that is not a cloud weather service (local station, gateway, passthrough,
/// demo, mapping source). The single source of truth for the per-service copy,
/// keyed off the `SourceKind` variant so a new cloud kind is a compile-checked
/// match arm here.
pub fn cloud_meta(kind: &SourceKind) -> Option<CloudSourceMeta> {
    use SourceKind::*;
    let meta = match kind {
        // NWS: a real instrument observation. Highest honesty: it is the only
        // kind here that is an actual measurement, not a model or analysis.
        Nws(_) => CloudSourceMeta {
            kind: "nws",
            data_nature: CloudDataNature::Observation,
            // NWS rain is a REAL station gauge measurement (precipitationLastHour).
            rain_nature: CloudDataNature::Observation,
            real_time: "Real station report, refreshed about hourly, can lag 30 to 90 minutes.",
            localization: "Nearest official station, often an airport 5 to 30 miles away.",
            watering_risk:
                "A real measurement, but from a station that may miss the rain on your yard.",
            key_tier: KeyTier::NoKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 60,
            // Just under a LAN gauge (100) and the radar QPE (90): a real
            // station observation is the second-best off-yard rain decision.
            irrigation_rank: 80,
            upgrade_reason: None,
        },
        // CLOUD WEATHER STATION tier. A real consumer station the user owns,
        // reached through the vendor cloud. Honestly an `Observation` (every
        // field, rain included, is a real instrument reading), so it ranks high on
        // honesty: just under NWS (an OFFICIAL station) because it depends on a
        // vendor account + cloud round-trip rather than a public keyless feed.
        // Ambient Weather (WS-2902 / WS-5000 family, api.ambientweather.net).
        AmbientWeather(_) => CloudSourceMeta {
            kind: "ambient_weather",
            data_nature: CloudDataNature::Observation,
            // It is YOUR station's real gauge: every field, rain included, is a
            // measurement.
            rain_nature: CloudDataNature::Observation,
            real_time:
                "Your own Ambient Weather station, polled about every minute through their cloud.",
            localization: "Your exact yard: it is your physical station, not a grid estimate.",
            watering_risk:
                "A real on-site measurement, the same gauge a direct LAN hookup would read, just routed through the vendor cloud.",
            key_tier: KeyTier::FreeKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            // Just under NWS (60): a real station you own, but cloud-routed and
            // account-gated rather than a public keyless feed.
            honesty_rank: 59,
            // Your own station gauge, cloud-routed: the best off-yard read short of
            // a direct-LAN gauge (100), above the radar QPE (90) and NWS (80).
            irrigation_rank: 85,
            upgrade_reason: None,
        },
        // Netatmo Weather Station (OAuth2, api.netatmo.com getstationsdata).
        Netatmo(_) => CloudSourceMeta {
            kind: "netatmo",
            data_nature: CloudDataNature::Observation,
            rain_nature: CloudDataNature::Observation,
            real_time:
                "Your own Netatmo station, read from their cloud as its modules report.",
            localization: "Your exact yard: it is your physical station, not a grid estimate.",
            watering_risk:
                "A real on-site measurement from your own station, routed through the Netatmo cloud.",
            key_tier: KeyTier::FreeKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 58,
            irrigation_rank: 85,
            upgrade_reason: None,
        },
        // La Crosse View (account login, ingv2.lacrossetechnology.com).
        Lacrosse(_) => CloudSourceMeta {
            kind: "lacrosse",
            data_nature: CloudDataNature::Observation,
            rain_nature: CloudDataNature::Observation,
            real_time:
                "Your own La Crosse station, read from the La Crosse View cloud as it reports.",
            localization: "Your exact yard: it is your physical station, not a grid estimate.",
            watering_risk:
                "A real on-site measurement from your own station, routed through the La Crosse View cloud.",
            key_tier: KeyTier::FreeKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 57,
            irrigation_rank: 85,
            upgrade_reason: None,
        },
        // Synoptic (MesoWest): a REAL instrument observation from the nearest
        // reporting station in Synoptic's dense mesonet (NWS/FAA + RAWS +
        // regional mesonets + personal stations). An honest Observation like
        // NWS, but usually a closer station; keyed by a free token rather than
        // keyless, so it ranks just under NWS for honesty. It emits wind /
        // pressure / temp / humidity (no rain gauge in the requested var set),
        // so its rain_nature never asserts a measurement it does not make.
        Synoptic(_) => CloudSourceMeta {
            kind: "synoptic",
            data_nature: CloudDataNature::Observation,
            // Synoptic is a real station report; but the requested variable set
            // (wind/pressure/temp/humidity) carries no rain gauge, so it does
            // not emit a current rain scalar (emits_current_rain = false). The
            // rain_nature stays Observation for the honest per-field contract
            // (a station reading), never a fabricated measurement.
            rain_nature: CloudDataNature::Observation,
            real_time:
                "Real station report from a dense mesonet, refreshed about every 10 minutes, can lag up to an hour.",
            localization:
                "Nearest reporting station in a dense mesonet, usually closer than the one official airport.",
            watering_risk:
                "A real measurement of wind, pressure, temperature and humidity, but it reports no rain gauge, so it never settles whether rain hit your yard.",
            key_tier: KeyTier::FreeKey,
            // No rain gauge in the requested variable set, so no current rain.
            emits_current_rain: false,
            pop_is_synthetic: false,
            // Just under NWS (60) and the cloud weather STATION tier (57 to 59,
            // your OWN station): a real station observation, but someone else's
            // station reached through a keyed feed.
            honesty_rank: 56,
            // A real station observation for wind/temp/pressure, but it reports
            // no rain, so its rain DECISION value is low: rank it below the rain
            // observation tiers (gauge, MRMS, NWS) and the model providers whose
            // rain it cannot beat, just above the coarse Met.no backstop.
            irrigation_rank: 35,
            upgrade_reason: None,
        },
        // NOAA MRMS: gauge-corrected radar QPE, observation-grade radar rain.
        // The best off-yard read of whether rain actually fell on your block,
        // short of your own gauge. Keyless, US-only.
        NoaaMrms(_) => CloudSourceMeta {
            kind: "noaa_mrms",
            data_nature: CloudDataNature::RadarQpe,
            rain_nature: CloudDataNature::RadarQpe,
            real_time:
                "Instant radar rain rate refreshed every couple minutes, plus a gauge-corrected hourly rainfall total.",
            localization:
                "A 1 km national radar grid, sees the rain on your block, not just a distant airport.",
            watering_risk:
                "The best off-yard read of whether rain actually fell here, short of your own gauge: a live radar rate plus an accurate hourly total.",
            key_tier: KeyTier::NoKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            // Just under NWS for presentation order (radar QPE is observation
            // grade but not a direct instrument report at your location).
            honesty_rank: 55,
            // Best off-yard rain decision short of a local gauge: above NWS.
            irrigation_rank: 90,
            upgrade_reason: None,
        },
        // Pirate Weather: its temp/wind current reads are a fast nowcast, but its
        // RAIN is HRRR/GEFS MODEL output, not radar (the mislabel fix). So the
        // headline data_nature stays Nowcast (temp/wind), while rain_nature is
        // honestly Forecast, and the watering copy says so.
        PirateWeather(_) => CloudSourceMeta {
            kind: "pirate_weather",
            data_nature: CloudDataNature::Nowcast,
            // Pirate's rain is HRRR/GEFS model output, NOT a radar measurement.
            rain_nature: CloudDataNature::Forecast,
            real_time:
                "A 15-minute analysis from live radar and station reports, only seconds of lag in the US.",
            localization:
                "About a 3 km grid in the US, catches a local cell far better than one distant station.",
            watering_risk:
                "Its rain is a model forecast, not a measurement, so a real gauge still settles whether rain hit your yard.",
            key_tier: KeyTier::FreeKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 50,
            // Rain is a model forecast, so it ranks below the real observation
            // tiers (gauge, MRMS, NWS) but above the coarser global models.
            irrigation_rank: 70,
            // CONUS upgrade note: the free key still sharpens temp/wind even
            // though the rain is a forecast.
            upgrade_reason: Some(
                "A free key sharpens the live temp and wind reads in the US, even though its rain is a forecast.",
            ),
        },
        // OpenWeather: a model blend recomputed about every 10 minutes. Paid
        // (credit card required).
        OpenWeather(_) => CloudSourceMeta {
            kind: "openweather",
            data_nature: CloudDataNature::Forecast,
            rain_nature: CloudDataNature::Forecast,
            real_time: "A blended estimate recomputed about every 10 minutes.",
            localization:
                "A roughly 500 m to 2 km cell, close to your yard, but still computed.",
            watering_risk:
                "Hyperlocal estimate, but a real gauge still settles whether rain hit your yard.",
            key_tier: KeyTier::Paid,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 40,
            irrigation_rank: 50,
            upgrade_reason: None,
        },
        // Apple WeatherKit: an ML blend, the most location-precise cloud option,
        // but still an algorithm. Paid (Apple Developer, 99 dollars a year).
        WeatherKit(_) => CloudSourceMeta {
            kind: "weatherkit",
            data_nature: CloudDataNature::Forecast,
            rain_nature: CloudDataNature::Forecast,
            real_time: "A blended estimate refreshed frequently through the day.",
            localization:
                "The most location-precise cloud option, tuned to your coordinates, but still an algorithm.",
            watering_risk:
                "Most precise cloud estimate, but still a prediction, so an on-site gauge can disagree.",
            key_tier: KeyTier::Paid,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 30,
            irrigation_rank: 60,
            upgrade_reason: None,
        },
        // Open-Meteo: the keyless model backstop. A model value for the current
        // interval, never a measurement.
        OpenMeteo(_) => CloudSourceMeta {
            kind: "open_meteo",
            data_nature: CloudDataNature::Forecast,
            rain_nature: CloudDataNature::Forecast,
            real_time:
                "A model value for the current interval, refreshed every 1 to 6 hours depending on region.",
            localization:
                "A model grid cell roughly 2 to 13 km near you, never a direct measurement.",
            watering_risk:
                "Treat its rain as a forecast, not proof: it can report rain that did not fall or miss a small cell.",
            key_tier: KeyTier::NoKey,
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 20,
            irrigation_rank: 40,
            upgrade_reason: None,
        },
        // Met.no: a coarse model now-step with a FABRICATED rain probability and
        // NO current observation. Lowest honesty; weakest for a US yard.
        MetNorway(_) => CloudSourceMeta {
            kind: "met_norway",
            data_nature: CloudDataNature::Forecast,
            rain_nature: CloudDataNature::Forecast,
            real_time:
                "A model value for the current hour, no observation, refreshed about hourly.",
            localization:
                "About 2.5 km in the Nordics, but a coarse 9 km or more for a US yard.",
            watering_risk:
                "Weakest for a US yard, coarse grid and a fabricated rain probability, use only as a forecast backup.",
            key_tier: KeyTier::NoKey,
            emits_current_rain: false,
            pop_is_synthetic: true,
            honesty_rank: 10,
            // Lowest rain-decision rank: coarse model + a synthetic POP.
            irrigation_rank: 30,
            upgrade_reason: None,
        },
        // Not a cloud weather service: no catalog metadata.
        _ => return None,
    };
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cloud_kind_has_metadata() {
        // The honesty catalog must cover exactly the eleven cloud weather kinds
        // (six forecast kinds, NOAA MRMS radar QPE, Synoptic mesonet
        // observation, and the three-member cloud weather STATION tier: Ambient
        // Weather, Netatmo, LaCrosse), and each must round-trip its own
        // variant-keyed lookup.
        let kinds = cloud_kinds();
        assert_eq!(kinds.len(), 11, "eleven cloud weather kinds in the catalog");
        for k in &kinds {
            let meta = cloud_meta(k).expect("every cloud kind has catalog metadata");
            assert_eq!(
                meta.kind,
                crate::config::kind_labels::source_kind_label(k),
                "catalog kind tag matches the canonical kind label"
            );
            // No em dashes anywhere in the honest copy (absolute rule).
            let mut lines = vec![meta.real_time, meta.localization, meta.watering_risk];
            if let Some(up) = meta.upgrade_reason {
                lines.push(up);
            }
            for line in lines {
                assert!(!line.contains('\u{2014}'), "no em dash in honest copy");
                assert!(!line.contains('\u{2013}'), "no en dash in honest copy");
            }
        }
    }

    #[test]
    fn rain_nature_is_honest_per_contract() {
        // The honest rain_nature: NWS observation, NOAA MRMS radar QPE, every
        // model provider (including Pirate, the mislabel fix) forecast.
        let by = |tag: &str| {
            cloud_kinds()
                .into_iter()
                .map(|k| cloud_meta(&k).unwrap())
                .find(|m| m.kind == tag)
                .unwrap()
        };
        assert_eq!(by("nws").rain_nature, CloudDataNature::Observation);
        assert_eq!(by("noaa_mrms").rain_nature, CloudDataNature::RadarQpe);
        // Pirate MISLABEL FIX: its rain is a model forecast, not radar/nowcast.
        assert_eq!(by("pirate_weather").rain_nature, CloudDataNature::Forecast);
        for tag in ["openweather", "weatherkit", "open_meteo", "met_norway"] {
            assert_eq!(
                by(tag).rain_nature,
                CloudDataNature::Forecast,
                "{tag} rain is a forecast"
            );
        }
        // Only Pirate carries the CONUS free-key upgrade note.
        assert!(by("pirate_weather").upgrade_reason.is_some());
        for tag in [
            "nws",
            "noaa_mrms",
            "openweather",
            "weatherkit",
            "open_meteo",
            "met_norway",
        ] {
            assert!(
                by(tag).upgrade_reason.is_none(),
                "{tag} has no upgrade note"
            );
        }
    }

    #[test]
    fn irrigation_rank_puts_radar_above_station_above_models() {
        // Best off-yard rain DECISION order: NOAA MRMS (radar QPE) > NWS
        // (station observation) > Pirate > WeatherKit > OpenWeather > Open-Meteo
        // > Met.no. A local gauge (~100, ranked elsewhere) outranks them all.
        let rank = |tag: &str| {
            cloud_kinds()
                .into_iter()
                .map(|k| cloud_meta(&k).unwrap())
                .find(|m| m.kind == tag)
                .unwrap()
                .irrigation_rank
        };
        assert!(rank("noaa_mrms") > rank("nws"));
        assert!(rank("nws") > rank("pirate_weather"));
        assert!(rank("pirate_weather") > rank("weatherkit"));
        assert!(rank("weatherkit") > rank("openweather"));
        assert!(rank("openweather") > rank("open_meteo"));
        assert!(rank("open_meteo") > rank("met_norway"));
        // Every cloud rain rank sits below a LAN gauge (100).
        for tag in ["noaa_mrms", "nws", "pirate_weather", "open_meteo"] {
            assert!(rank(tag) < 100, "{tag} ranks below a local gauge");
        }
    }

    #[test]
    fn non_cloud_kind_has_no_metadata() {
        // A local station / passthrough / demo is not a cloud weather service.
        use crate::config::schema::{HaPassthroughConfig, TempestUdpConfig};
        assert!(cloud_meta(&SourceKind::TempestUdp(TempestUdpConfig {
            bind_addr: "0.0.0.0:50222".into(),
            hub_serial: None,
        }))
        .is_none());
        assert!(cloud_meta(&SourceKind::HaPassthrough(HaPassthroughConfig {
            base_url: "http://ha.local:8123".into(),
            bearer_token: String::new(),
            field_map: Default::default(),
            soil_zone_map: Default::default(),
        }))
        .is_none());
    }

    #[test]
    fn honesty_rank_orders_nws_highest_metno_lowest() {
        // The audit ranking: NWS > Pirate > OpenWeather > WeatherKit >
        // Open-Meteo > Met.no. Verify the strict descending order.
        let ranks: Vec<i32> = cloud_kinds()
            .iter()
            .map(|k| cloud_meta(k).unwrap().honesty_rank)
            .collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            ranks, sorted,
            "cloud_kinds() is already highest-honesty-first"
        );
        // NWS strictly highest, Met.no strictly lowest.
        let nws = cloud_meta(&cloud_kinds()[0]).unwrap();
        assert_eq!(nws.kind, "nws");
        assert_eq!(nws.honesty_rank, *sorted.first().unwrap());
        let metno = cloud_kinds()
            .into_iter()
            .map(|k| cloud_meta(&k).unwrap())
            .min_by_key(|m| m.honesty_rank)
            .unwrap();
        assert_eq!(metno.kind, "met_norway");
    }

    #[test]
    fn emits_current_rain_and_pop_reflect_post_fix_state() {
        // POST-FIX: every kind emits a current rain scalar EXCEPT Met.no (no
        // current observation at all) and Synoptic (a station observation whose
        // requested variable set carries no rain gauge). Only Met.no synthesizes
        // its POP.
        for k in cloud_kinds() {
            let m = cloud_meta(&k).unwrap();
            match m.kind {
                "met_norway" => {
                    assert!(!m.emits_current_rain, "Met.no emits no current rain");
                    assert!(m.pop_is_synthetic, "Met.no POP is synthetic");
                }
                "synoptic" => {
                    assert!(
                        !m.emits_current_rain,
                        "Synoptic emits no current rain (no gauge in the requested vars)"
                    );
                    assert!(!m.pop_is_synthetic, "Synoptic has no POP to synthesize");
                }
                _ => {
                    assert!(m.emits_current_rain, "{} emits current rain", m.kind);
                    assert!(!m.pop_is_synthetic, "{} POP is real", m.kind);
                }
            }
        }
    }

    #[test]
    fn data_nature_and_key_tier_serialize_snake_case() {
        // The UI contract: the enum wire strings.
        assert_eq!(
            serde_json::to_value(CloudDataNature::Observation).unwrap(),
            serde_json::json!("observation")
        );
        assert_eq!(
            serde_json::to_value(CloudDataNature::RadarQpe).unwrap(),
            serde_json::json!("radar_qpe")
        );
        assert_eq!(
            serde_json::to_value(CloudDataNature::Nowcast).unwrap(),
            serde_json::json!("nowcast")
        );
        assert_eq!(
            serde_json::to_value(CloudDataNature::Forecast).unwrap(),
            serde_json::json!("forecast")
        );
        assert_eq!(
            serde_json::to_value(KeyTier::NoKey).unwrap(),
            serde_json::json!("no_key")
        );
        assert_eq!(
            serde_json::to_value(KeyTier::FreeKey).unwrap(),
            serde_json::json!("free_key")
        );
        assert_eq!(
            serde_json::to_value(KeyTier::Paid).unwrap(),
            serde_json::json!("paid")
        );
    }

    /// Resolve a `CloudSourceMeta` by its canonical kind tag for the per-field
    /// nature assertions below.
    fn meta_by(tag: &str) -> CloudSourceMeta {
        cloud_kinds()
            .into_iter()
            .map(|k| cloud_meta(&k).unwrap())
            .find(|m| m.kind == tag)
            .unwrap_or_else(|| panic!("no catalog meta for {tag}"))
    }

    #[test]
    fn field_nature_splits_pirate_nowcast_wind_from_forecast_rain() {
        // THE per-field honesty the single overall data_nature cannot express:
        // Pirate's wind/temp/pressure/UV are a live NOWCAST while its rain is a
        // model FORECAST. The matrix renders these as different-colored cells in
        // the same row, the directive-5 truth the one rain badge buries.
        let pirate = meta_by("pirate_weather");
        // Non-rain fields inherit the headline Nowcast.
        for field in [
            "wind_mph",
            "wind_gust_mph",
            "wind_bearing_deg",
            "air_temp_f",
            "dew_point_f",
            "rh_pct",
            "pressure_in_hg",
            "uv_index",
        ] {
            assert_eq!(
                pirate.field_nature(field),
                CloudDataNature::Nowcast,
                "Pirate {field} is a live nowcast"
            );
        }
        // Rain fields track the honest rain_nature override: a model Forecast.
        for field in ["rain_today_in", "rain_intensity_in_hr", "pop"] {
            assert_eq!(
                pirate.field_nature(field),
                CloudDataNature::Forecast,
                "Pirate {field} is a model forecast, never a nowcast"
            );
        }
    }

    #[test]
    fn field_nature_rain_keys_track_rain_nature_per_kind() {
        // For every cloud kind, the rain fields resolve to the per-row rain_nature
        // override (NOT the headline data_nature), and every NON-rain field
        // resolves to data_nature EXCEPT Open-Meteo's modeled ET0, which is an
        // explicit Forecast override.
        for k in cloud_kinds() {
            let m = cloud_meta(&k).unwrap();
            for rain in ["rain_today_in", "rain_intensity_in_hr", "pop"] {
                assert_eq!(
                    m.field_nature(rain),
                    m.rain_nature,
                    "{} rain field {rain} tracks rain_nature",
                    m.kind
                );
            }
            // A representative non-rain field inherits the headline nature.
            assert_eq!(
                m.field_nature("wind_mph"),
                m.data_nature,
                "{} non-rain field inherits data_nature",
                m.kind
            );
        }
        // Open-Meteo's native ET0 is a modeled daily total: an explicit Forecast.
        assert_eq!(
            meta_by("open_meteo").field_nature("et0_today"),
            CloudDataNature::Forecast,
            "Open-Meteo ET0 is a modeled forecast, never an observation"
        );
    }

    #[test]
    fn cloud_weather_station_tier_is_observation_with_all_obs_fields() {
        // The surfaced PWS adapters (ambient_weather / netatmo / lacrosse) appear
        // in cloud_kinds() as a distinct OBSERVATION tier: every field, rain
        // included, is a real station measurement.
        let tags: Vec<&str> = cloud_kinds()
            .iter()
            .map(|k| cloud_meta(k).unwrap().kind)
            .collect();
        for tag in ["ambient_weather", "netatmo", "lacrosse"] {
            assert!(
                tags.contains(&tag),
                "PWS station kind {tag} is present in cloud_kinds()"
            );
            let m = meta_by(tag);
            assert_eq!(
                m.data_nature,
                CloudDataNature::Observation,
                "{tag} is an Observation tier"
            );
            assert_eq!(
                m.rain_nature,
                CloudDataNature::Observation,
                "{tag} rain is a real measurement"
            );
            // Every field the tier can emit resolves Observation (no overrides).
            for field in [
                "air_temp_f",
                "wind_mph",
                "wind_gust_mph",
                "pressure_in_hg",
                "uv_index",
                "rain_today_in",
                "rain_intensity_in_hr",
            ] {
                assert_eq!(
                    m.field_nature(field),
                    CloudDataNature::Observation,
                    "{tag} {field} is a station observation"
                );
            }
        }
    }
}
